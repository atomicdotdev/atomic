//! Local Git recovery snapshots for unexplained tracked working-copy bytes.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Namespace reserved for local, non-publishable recovery refs.
pub const WIP_REF_PREFIX: &str = "refs/atomic/wip/";

const RECOVERY_REFLOG_MESSAGE: &str = "atomic wip recovery";
const RECOVERY_COMMIT_MESSAGE: &str = "Atomic WIP recovery snapshot\n\nThis commit preserves tracked repository bytes only.\nIt does not assert authorship or pre-checkout provenance.\n";

/// Inputs that identify one immutable recovery operation.
///
/// `workspace` and `operation` are hashed before entering the ref name. Callers
/// must use a distinct operation identity when they intend to preserve a new
/// snapshot; an existing derived ref is never overwritten.
#[derive(Clone, Copy, Debug)]
pub struct WipCaptureRequest<'a> {
    pub repository_root: &'a Path,
    pub workspace: &'a str,
    pub operation: &'a str,
}

impl<'a> WipCaptureRequest<'a> {
    pub fn new(repository_root: &'a Path, workspace: &'a str, operation: &'a str) -> Self {
        Self {
            repository_root,
            workspace,
            operation,
        }
    }
}

/// A completed tracked-byte recovery capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WipCapture {
    pub ref_name: String,
    pub commit_oid: String,
    pub tree_oid: String,
    /// The Git commit observed before capture and used as the recovery commit's
    /// sole parent. `None` is reserved for an unborn repository.
    pub parent_oid: Option<String>,
    /// Raw Git paths whose repository representation differs from the observed
    /// parent. No lossy path decoding is performed.
    pub paths: Vec<Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
pub enum WipCaptureError {
    #[error("{field} must not be empty")]
    EmptyIdentifier { field: &'static str },

    #[error("'{path}' is not a Git working tree")]
    NotAWorkingTree { path: PathBuf },

    #[error("Git HEAD changed during WIP capture ({before:?} -> {after:?})")]
    HeadChanged {
        before: Option<String>,
        after: Option<String>,
    },

    #[error("WIP recovery ref '{ref_name}' already exists; refusing to overwrite it")]
    RefAlreadyExists { ref_name: String },

    #[error("existing WIP recovery ref '{ref_name}' cannot be reused: {detail}")]
    ExistingRefMismatch { ref_name: String, detail: String },

    #[error("cannot decode Git output while {operation}: {source}")]
    InvalidOutput {
        operation: &'static str,
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("cannot {operation}: {source}")]
    CommandIo {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("Git failed while {operation} (exit {status:?}): {stderr}")]
    GitCommand {
        operation: &'static str,
        status: Option<i32>,
        stderr: String,
    },

    #[error("unsupported Git object format '{format}'")]
    UnsupportedObjectFormat { format: String },

    #[error("cannot prepare alternate Git index: {0}")]
    TempIndex(#[source] std::io::Error),
}

/// Whether `ref_name` is in Atomic's local WIP namespace.
pub fn is_wip_ref(ref_name: &[u8]) -> bool {
    ref_name.starts_with(WIP_REF_PREFIX.as_bytes())
}

/// Central publication predicate for Git refs managed by Atomic.
pub fn is_publishable_git_ref(ref_name: &[u8]) -> bool {
    !is_wip_ref(ref_name)
}

/// Preserve the current tracked working tree under an immutable local Git ref.
///
/// The live index is only read. A temporary alternate index is initialized from
/// the observed `HEAD`, then updated with the union of `HEAD` and live-index
/// paths. Git therefore applies normal clean filters and records file modes,
/// symlinks, deletions, and zero-byte blobs without adding unrelated untracked
/// files. The resulting recovery commit has the observed current `HEAD` as its
/// only parent and carries no Atomic change, view, agent, or provenance claims.
pub fn capture_tracked_wip(request: WipCaptureRequest<'_>) -> Result<WipCapture, WipCaptureError> {
    validate_identifier("workspace", request.workspace)?;
    validate_identifier("operation", request.operation)?;
    let prepared = prepare_tracked_wip(request.repository_root)?;
    create_prepared_capture(request, prepared)
}

/// Preserve tracked bytes or safely reuse the immutable ref from an earlier
/// attempt of the same operation.
///
/// Reuse is accepted only when the existing ref resolves to a neutral Atomic
/// recovery commit with the same current HEAD parent and the same freshly
/// computed repository-byte tree. A changed working tree therefore cannot be
/// mistaken for the already-preserved snapshot.
pub fn capture_or_reuse_tracked_wip(
    request: WipCaptureRequest<'_>,
) -> Result<WipCapture, WipCaptureError> {
    validate_identifier("workspace", request.workspace)?;
    validate_identifier("operation", request.operation)?;
    let prepared = prepare_tracked_wip(request.repository_root)?;
    let ref_name = recovery_ref_name(request.workspace, request.operation);

    if let Some(commit_oid) = resolve_ref(request.repository_root, &ref_name)? {
        return reuse_prepared_capture(request.repository_root, ref_name, commit_oid, prepared);
    }

    match create_prepared_capture(request, prepared.clone()) {
        Ok(capture) => Ok(capture),
        // Another process may win the create-only update-ref after our initial
        // lookup. Verify its immutable result against the prepared bytes.
        Err(WipCaptureError::RefAlreadyExists { ref_name }) => {
            let commit_oid = resolve_ref(request.repository_root, &ref_name)?.ok_or_else(|| {
                WipCaptureError::ExistingRefMismatch {
                    ref_name: ref_name.clone(),
                    detail: "the ref disappeared during retry verification".to_string(),
                }
            })?;
            reuse_prepared_capture(request.repository_root, ref_name, commit_oid, prepared)
        }
        Err(error) => Err(error),
    }
}

#[derive(Clone, Debug)]
struct PreparedWip {
    parent_oid: Option<String>,
    tree_oid: String,
    paths: Vec<Vec<u8>>,
}

fn prepare_tracked_wip(root: &Path) -> Result<PreparedWip, WipCaptureError> {
    require_worktree(root)?;
    let parent_oid = resolve_head(root)?;
    let tracked_paths = collect_tracked_paths(root, parent_oid.as_deref())?;
    let temp_index = TempIndex::new()?;

    initialize_alternate_index(root, &temp_index.path, parent_oid.as_deref())?;
    update_alternate_index(root, &temp_index.path, &tracked_paths)?;
    let tree_oid = git_text(
        root,
        "write WIP tree",
        &os_args(&["write-tree"]),
        Some(&temp_index.path),
        None,
    )?;
    let paths = changed_paths(root, parent_oid.as_deref(), &tree_oid)?;
    ensure_head_unchanged(root, &parent_oid)?;

    Ok(PreparedWip {
        parent_oid,
        tree_oid,
        paths,
    })
}

fn create_prepared_capture(
    request: WipCaptureRequest<'_>,
    prepared: PreparedWip,
) -> Result<WipCapture, WipCaptureError> {
    ensure_head_unchanged(request.repository_root, &prepared.parent_oid)?;
    let commit_oid = create_recovery_commit(
        request.repository_root,
        &prepared.tree_oid,
        prepared.parent_oid.as_deref(),
    )?;
    ensure_head_unchanged(request.repository_root, &prepared.parent_oid)?;

    let ref_name = recovery_ref_name(request.workspace, request.operation);
    create_recovery_ref(request.repository_root, &ref_name, &commit_oid)?;
    Ok(WipCapture {
        ref_name,
        commit_oid,
        tree_oid: prepared.tree_oid,
        parent_oid: prepared.parent_oid,
        paths: prepared.paths,
    })
}

fn reuse_prepared_capture(
    root: &Path,
    ref_name: String,
    commit_oid: String,
    prepared: PreparedWip,
) -> Result<WipCapture, WipCaptureError> {
    ensure_head_unchanged(root, &prepared.parent_oid)?;
    let actual_tree = git_text(
        root,
        "read existing WIP tree",
        &[
            OsString::from("show"),
            OsString::from("-s"),
            OsString::from("--format=%T"),
            OsString::from(&commit_oid),
        ],
        None,
        None,
    )?;
    if actual_tree != prepared.tree_oid {
        return Err(WipCaptureError::ExistingRefMismatch {
            ref_name,
            detail: format!(
                "tree differs (existing {actual_tree}, current {})",
                prepared.tree_oid
            ),
        });
    }

    let parents = git_text(
        root,
        "read existing WIP parents",
        &[
            OsString::from("show"),
            OsString::from("-s"),
            OsString::from("--format=%P"),
            OsString::from(&commit_oid),
        ],
        None,
        None,
    )?;
    let actual_parents: Vec<_> = parents.split_whitespace().collect();
    let expected_parents: Vec<_> = prepared.parent_oid.iter().map(String::as_str).collect();
    if actual_parents != expected_parents {
        return Err(WipCaptureError::ExistingRefMismatch {
            ref_name,
            detail: format!(
                "parent differs (existing {:?}, expected {:?})",
                actual_parents, expected_parents
            ),
        });
    }

    let identity_and_message = git_text(
        root,
        "verify existing WIP identity",
        &[
            OsString::from("show"),
            OsString::from("-s"),
            OsString::from("--format=%an%x00%ae%x00%cn%x00%ce%x00%B"),
            OsString::from(&commit_oid),
        ],
        None,
        None,
    )?;
    let fields: Vec<_> = identity_and_message.splitn(5, '\0').collect();
    let neutral = fields.len() == 5
        && fields[0] == "Atomic Recovery"
        && fields[1] == "recovery@atomic.invalid"
        && fields[2] == "Atomic Recovery"
        && fields[3] == "recovery@atomic.invalid"
        && fields[4].starts_with(
            "Atomic WIP recovery snapshot\n\nThis commit preserves tracked repository bytes only.",
        );
    if !neutral {
        return Err(WipCaptureError::ExistingRefMismatch {
            ref_name,
            detail: "commit is not a neutral Atomic recovery object".to_string(),
        });
    }

    ensure_head_unchanged(root, &prepared.parent_oid)?;
    Ok(WipCapture {
        ref_name,
        commit_oid,
        tree_oid: prepared.tree_oid,
        parent_oid: prepared.parent_oid,
        paths: prepared.paths,
    })
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), WipCaptureError> {
    if value.trim().is_empty() {
        return Err(WipCaptureError::EmptyIdentifier { field });
    }
    Ok(())
}

fn recovery_ref_name(workspace: &str, operation: &str) -> String {
    format!(
        "{WIP_REF_PREFIX}workspace-{}/operation-{}",
        short_digest(workspace),
        short_digest(operation)
    )
}

fn short_digest(value: &str) -> String {
    let digest = blake3::hash(value.as_bytes()).to_hex().to_string();
    digest[..20].to_string()
}

fn require_worktree(root: &Path) -> Result<(), WipCaptureError> {
    let inside = git_text(
        root,
        "identify Git working tree",
        &os_args(&["rev-parse", "--is-inside-work-tree"]),
        None,
        None,
    )?;
    if inside != "true" {
        return Err(WipCaptureError::NotAWorkingTree {
            path: root.to_path_buf(),
        });
    }
    Ok(())
}

fn resolve_head(root: &Path) -> Result<Option<String>, WipCaptureError> {
    let output = run_git_raw(
        root,
        "resolve Git HEAD",
        &os_args(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
        None,
        None,
    )?;
    if output.status.success() {
        return output_text("resolve Git HEAD", output.stdout).map(Some);
    }
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        return Ok(None);
    }
    Err(command_failure("resolve Git HEAD", &output))
}

fn collect_tracked_paths(
    root: &Path,
    parent_oid: Option<&str>,
) -> Result<BTreeSet<Vec<u8>>, WipCaptureError> {
    let mut paths = split_nul(git_bytes(
        root,
        "list live-index paths",
        &os_args(&["ls-files", "--cached", "-z"]),
        None,
        None,
    )?)
    .into_iter()
    .collect::<BTreeSet<_>>();

    if let Some(parent_oid) = parent_oid {
        let args = vec![
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("--name-only"),
            OsString::from(parent_oid),
        ];
        paths.extend(split_nul(git_bytes(
            root,
            "list HEAD paths",
            &args,
            None,
            None,
        )?));
    }

    Ok(paths)
}

fn initialize_alternate_index(
    root: &Path,
    index: &Path,
    parent_oid: Option<&str>,
) -> Result<(), WipCaptureError> {
    let args = match parent_oid {
        Some(parent_oid) => vec![OsString::from("read-tree"), OsString::from(parent_oid)],
        None => os_args(&["read-tree", "--empty"]),
    };
    git_bytes(root, "initialize alternate index", &args, Some(index), None)?;
    Ok(())
}

fn update_alternate_index(
    root: &Path,
    index: &Path,
    paths: &BTreeSet<Vec<u8>>,
) -> Result<(), WipCaptureError> {
    if paths.is_empty() {
        return Ok(());
    }

    let mut pathspec = Vec::new();
    for path in paths {
        pathspec.extend_from_slice(path);
        pathspec.push(0);
    }
    git_bytes(
        root,
        "apply tracked working tree to alternate index",
        &os_args(&[
            "--literal-pathspecs",
            "add",
            "--all",
            "--force",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]),
        Some(index),
        Some(&pathspec),
    )?;
    Ok(())
}

fn changed_paths(
    root: &Path,
    parent_oid: Option<&str>,
    tree_oid: &str,
) -> Result<Vec<Vec<u8>>, WipCaptureError> {
    let bytes = if let Some(parent_oid) = parent_oid {
        let args = vec![
            OsString::from("diff-tree"),
            OsString::from("--no-commit-id"),
            OsString::from("--name-only"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from(parent_oid),
            OsString::from(tree_oid),
        ];
        git_bytes(root, "list WIP paths", &args, None, None)?
    } else {
        let args = vec![
            OsString::from("ls-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("--name-only"),
            OsString::from(tree_oid),
        ];
        git_bytes(root, "list unborn WIP paths", &args, None, None)?
    };
    Ok(split_nul(bytes))
}

fn ensure_head_unchanged(root: &Path, before: &Option<String>) -> Result<(), WipCaptureError> {
    let after = resolve_head(root)?;
    if &after != before {
        return Err(WipCaptureError::HeadChanged {
            before: before.clone(),
            after,
        });
    }
    Ok(())
}

fn create_recovery_commit(
    root: &Path,
    tree_oid: &str,
    parent_oid: Option<&str>,
) -> Result<String, WipCaptureError> {
    let mut args = vec![OsString::from("commit-tree"), OsString::from(tree_oid)];
    if let Some(parent_oid) = parent_oid {
        args.push(OsString::from("-p"));
        args.push(OsString::from(parent_oid));
    }

    let mut command = git_command(root, &args, None);
    command
        .env("GIT_AUTHOR_NAME", "Atomic Recovery")
        .env("GIT_AUTHOR_EMAIL", "recovery@atomic.invalid")
        .env("GIT_COMMITTER_NAME", "Atomic Recovery")
        .env("GIT_COMMITTER_EMAIL", "recovery@atomic.invalid");
    let output = execute_raw(
        command,
        "create neutral WIP commit",
        Some(RECOVERY_COMMIT_MESSAGE.as_bytes()),
    )?;
    if !output.status.success() {
        return Err(command_failure("create neutral WIP commit", &output));
    }
    output_text("create neutral WIP commit", output.stdout)
}

fn create_recovery_ref(
    root: &Path,
    ref_name: &str,
    commit_oid: &str,
) -> Result<(), WipCaptureError> {
    let object_format = git_text(
        root,
        "read Git object format",
        &os_args(&["rev-parse", "--show-object-format"]),
        None,
        None,
    )?;
    let zero_oid = match object_format.as_str() {
        "sha1" => "0".repeat(40),
        "sha256" => "0".repeat(64),
        format => {
            return Err(WipCaptureError::UnsupportedObjectFormat {
                format: format.to_string(),
            })
        }
    };
    let args = vec![
        OsString::from("update-ref"),
        OsString::from("--create-reflog"),
        OsString::from("-m"),
        OsString::from(RECOVERY_REFLOG_MESSAGE),
        OsString::from(ref_name),
        OsString::from(commit_oid),
        OsString::from(zero_oid),
    ];
    let output = run_git_raw(root, "create WIP recovery ref", &args, None, None)?;
    if output.status.success() {
        return Ok(());
    }
    if resolve_ref(root, ref_name)?.is_some() {
        return Err(WipCaptureError::RefAlreadyExists {
            ref_name: ref_name.to_string(),
        });
    }
    Err(command_failure("create WIP recovery ref", &output))
}

fn resolve_ref(root: &Path, ref_name: &str) -> Result<Option<String>, WipCaptureError> {
    let args = vec![
        OsString::from("rev-parse"),
        OsString::from("--verify"),
        OsString::from("--quiet"),
        OsString::from(ref_name),
    ];
    let output = run_git_raw(root, "resolve WIP recovery ref", &args, None, None)?;
    if output.status.success() {
        return output_text("resolve WIP recovery ref", output.stdout).map(Some);
    }
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        return Ok(None);
    }
    Err(command_failure("resolve WIP recovery ref", &output))
}

fn split_nul(bytes: Vec<u8>) -> Vec<Vec<u8>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

fn os_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn git_text(
    root: &Path,
    operation: &'static str,
    args: &[OsString],
    index: Option<&Path>,
    input: Option<&[u8]>,
) -> Result<String, WipCaptureError> {
    output_text(operation, git_bytes(root, operation, args, index, input)?)
}

fn output_text(operation: &'static str, bytes: Vec<u8>) -> Result<String, WipCaptureError> {
    String::from_utf8(bytes)
        .map(|value| value.trim().to_string())
        .map_err(|source| WipCaptureError::InvalidOutput { operation, source })
}

fn git_bytes(
    root: &Path,
    operation: &'static str,
    args: &[OsString],
    index: Option<&Path>,
    input: Option<&[u8]>,
) -> Result<Vec<u8>, WipCaptureError> {
    let output = run_git_raw(root, operation, args, index, input)?;
    if !output.status.success() {
        return Err(command_failure(operation, &output));
    }
    Ok(output.stdout)
}

fn run_git_raw(
    root: &Path,
    operation: &'static str,
    args: &[OsString],
    index: Option<&Path>,
    input: Option<&[u8]>,
) -> Result<Output, WipCaptureError> {
    execute_raw(git_command(root, args, index), operation, input)
}

fn git_command(root: &Path, args: &[OsString], index: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
}

fn execute_raw(
    mut command: Command,
    operation: &'static str,
    input: Option<&[u8]>,
) -> Result<Output, WipCaptureError> {
    if let Some(input) = input {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| WipCaptureError::CommandIo { operation, source })?;
        child
            .stdin
            .take()
            .expect("piped Git stdin")
            .write_all(input)
            .map_err(|source| WipCaptureError::CommandIo { operation, source })?;
        return child
            .wait_with_output()
            .map_err(|source| WipCaptureError::CommandIo { operation, source });
    }

    command
        .output()
        .map_err(|source| WipCaptureError::CommandIo { operation, source })
}

fn command_failure(operation: &'static str, output: &Output) -> WipCaptureError {
    WipCaptureError::GitCommand {
        operation,
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

struct TempIndex {
    directory: PathBuf,
    path: PathBuf,
}

impl TempIndex {
    fn new() -> Result<Self, WipCaptureError> {
        let directory = std::env::temp_dir().join(format!(
            "atomic-git-wip-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).map_err(WipCaptureError::TempIndex)?;
        let path = directory.join("index");
        Ok(Self { directory, path })
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run Git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn initialized_repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        git_ok(root.path(), &["init", "-q"]);
        git_ok(root.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_ok(root.path(), &["config", "user.name", "Atomic Test"]);
        git_ok(root.path(), &["config", "user.email", "atomic@example.com"]);
        fs::write(root.path().join("tracked.txt"), b"one\n").expect("write tracked");
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "initial"]);
        root
    }

    #[test]
    fn capture_or_reuse_returns_identical_recovery_on_retry() {
        let root = initialized_repository();
        fs::write(root.path().join("tracked.txt"), b"two\n").expect("modify tracked");
        let request = WipCaptureRequest::new(root.path(), "workspace", "operation");

        let first = capture_or_reuse_tracked_wip(request).expect("first capture");
        let retry = capture_or_reuse_tracked_wip(request).expect("verified reuse");
        assert_eq!(retry, first);

        fs::write(root.path().join("tracked.txt"), b"three\n").expect("modify again");
        let error = capture_or_reuse_tracked_wip(request)
            .expect_err("changed bytes must not reuse the old recovery ref");
        assert!(matches!(error, WipCaptureError::ExistingRefMismatch { .. }));
    }
}
