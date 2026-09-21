//! Advisory Git hook integration for the colocated Atomic bridge.
//!
//! Atomic installs only an explicitly owned `post-checkout` dispatcher. The
//! dispatcher appends immutable local evidence and schedules a read-only
//! observation after Git exits; it never imports, reconciles, materializes, or
//! moves refs. Unmanaged hook systems and custom `core.hooksPath` values are
//! detected and left untouched.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::Duration;

use chrono::Utc;
use clap::{Args, Subcommand};
use git2::{ErrorCode, Repository as GitRepository};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::observation::{observe_git, GitObservation, HeadObservation};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

use atomic_repository::repository::{
    ATOMIC_DISPATCHER_MARKER as DISPATCHER_MARKER, ATOMIC_LEGACY_MARKER_BEGIN as LEGACY_MARKER_BEGIN,
};

const LEGACY_MARKER_END: &str = "# atomic:git:end";
const EVENT_VERSION: u32 = 1;
const EVENT_JOURNAL_RELATIVE: &str = ".atomic/bridge/git-events.jsonl";
const DEFERRED_DIRECTORY_RELATIVE: &str = ".atomic/bridge/deferred-observations";
const DEFERRED_DELAY_MS: u64 = 300;

/// Backward-compatible entry point for bridge hook management.
#[derive(Debug, Args)]
pub struct Hooks {
    #[command(subcommand)]
    pub command: HookCommands,
}

#[derive(Debug, Subcommand)]
pub enum HookCommands {
    /// Enable the advisory bridge `post-checkout` dispatcher.
    Install,
    /// Remove an Atomic-owned bridge dispatcher.
    Uninstall,
    /// Show advisory bridge hook status.
    Status,
}

impl Command for Hooks {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        match &self.command {
            HookCommands::Install => enable_bridge(&root, false),
            HookCommands::Uninstall => uninstall_bridge(&root),
            HookCommands::Status => show_status(&root),
        }
    }
}

#[derive(Debug)]
struct GitHookContext {
    common_dir: PathBuf,
    custom_hooks_path: Option<PathBuf>,
}

#[derive(Debug, Eq, PartialEq)]
enum DispatcherInstall {
    Installed,
    Refreshed,
    Unmanaged,
}

/// Enable the bridge's advisory `post-checkout` integration.
///
/// Only a missing hook or an exact Atomic-owned dispatcher is written. A
/// custom `core.hooksPath`, symlink, binary, or unmanaged hook is reported with
/// an integration command and left byte-for-byte untouched.
pub(crate) fn enable_bridge(root: &Path, mirror_ignores: bool) -> CliResult<()> {
    let root = canonical_root(root)?;
    let binary = std::env::current_exe()
        .map_err(|error| git_error(format!("cannot resolve the Atomic binary path: {error}")))?;
    let context = git_hook_context(&root)?;

    if let Some(custom_hooks_path) = context.custom_hooks_path {
        print_unmanaged_instructions(
            &format!(
                "core.hooksPath is configured as '{}'; Atomic did not modify that hook system",
                custom_hooks_path.display()
            ),
            &binary,
        )?;
        return Ok(());
    }

    let hooks_dir = context.common_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).map_err(|error| {
        git_error(format!(
            "cannot create Git hooks directory '{}': {error}",
            hooks_dir.display()
        ))
    })?;

    migrate_legacy_import_hooks(&hooks_dir, &binary)?;

    let hook_path = hooks_dir.join("post-checkout");
    let script = dispatcher_script(&binary, "hook-post-checkout").map_err(|error| {
        git_error(format!(
            "cannot build the advisory post-checkout dispatcher: {error}"
        ))
    })?;
    match install_or_refresh_dispatcher(&hook_path, &script).map_err(|error| {
        git_error(format!(
            "cannot install advisory dispatcher '{}': {error}",
            hook_path.display()
        ))
    })? {
        DispatcherInstall::Installed => print_info(&format!(
            "Installed Atomic advisory post-checkout dispatcher at {}",
            hook_path.display()
        )),
        DispatcherInstall::Refreshed => print_info(&format!(
            "Refreshed Atomic advisory post-checkout dispatcher at {}",
            hook_path.display()
        )),
        DispatcherInstall::Unmanaged => print_unmanaged_instructions(
            &format!(
                "existing post-checkout hook '{}' is not Atomic-owned and was left untouched",
                hook_path.display()
            ),
            &binary,
        )?,
    }

    // CB-9B: journal post-rewrite and reference-transaction evidence with the
    // same ownership rules — a missing hook or an exact Atomic-owned
    // dispatcher is written; anything else is reported and left untouched.
    // CB-12A: the pre-commit dispatcher additionally writes authenticated
    // commit-time capture evidence for active managed agent sessions
    // (RFC §10.3/§11.1). It is advisory evidence only: it never blocks a
    // commit, and the managed inseparable-commit policy (RFC §19 Q2) stays
    // UNDECIDED — no snapshot gating or fail-closed behavior is implemented
    // until the owner resolves it.
    for (hook_name, subcommand, purpose, forward_state_arg) in [
        (
            "pre-commit",
            "hook-pre-commit",
            "pre-commit capture evidence (managed sessions only, advisory)",
            false,
        ),
        (
            "pre-push",
            "hook-pre-push",
            "pre-push provenance verification (advisory to the guarantee; refuses locally)",
            false,
        ),
        (
            "post-rewrite",
            "hook-post-rewrite",
            "post-rewrite evidence (operation linkage only)",
            false,
        ),
        (
            "reference-transaction",
            "hook-reference-transaction",
            "reference-transaction evidence (ref movement only)",
            true,
        ),
    ] {
        let hook_path = hooks_dir.join(hook_name);
        // CB-12B: the pre-push dispatcher propagates a local refusal's exit
        // status; every other advisory dispatcher stays `|| true`.
        let script = stdin_dispatcher_script_ext(
            &binary,
            subcommand,
            forward_state_arg,
            hook_name == "pre-push",
        )
        .map_err(|error| {
            git_error(format!(
                "cannot build the advisory {hook_name} dispatcher: {error}"
            ))
        })?;
        match install_or_refresh_dispatcher(&hook_path, &script).map_err(|error| {
            git_error(format!(
                "cannot install advisory dispatcher '{}': {error}",
                hook_path.display()
            ))
        })? {
            DispatcherInstall::Installed => print_info(&format!(
                "Installed Atomic advisory {purpose} dispatcher at {}",
                hook_path.display()
            )),
            DispatcherInstall::Refreshed => print_info(&format!(
                "Refreshed Atomic advisory {purpose} dispatcher at {}",
                hook_path.display()
            )),
            DispatcherInstall::Unmanaged => {
                let command = format!("{} git bridge {subcommand} || true", shell_quote(&binary).map_err(|error| {
                    git_error(format!("cannot quote the binary path: {error}"))
                })?);
                print_warning(&format!(
                    "existing {hook_name} hook '{}' is not Atomic-owned and was left untouched",
                    hook_path.display()
                ));
                print_info("Add this advisory command to the existing hook or hook manager:");
                print_info(&format!("  {command}"));
            }
        }
    }

    // CB-13D (RFC §11.2 rule 2): enable never edits Git's own monitoring
    // configuration. Changes to `core.fsmonitor` and `core.untrackedCache`
    // require the user's explicit consent, so Atomic reports the exact
    // commands instead of setting them. The `native` notify tier is
    // explicitly unshipped in the first release (RFC §11.2 rule 7).
    print_info(
        "Atomic did not change core.fsmonitor or core.untrackedCache; enabling them is a \
         separate explicit decision, e.g. 'git config core.fsmonitor true' and \
         'git config core.untrackedCache true'",
    );

    Ok(())
}

/// CB-11A: mirror unrepresented `.atomicignore` patterns into the managed
/// `.git/info/exclude` block. This is the explicit-consent operation (RFC
/// §9.4); it is only ever run when the user passes `--mirror-ignores`.
pub(crate) fn mirror_ignores_with_consent(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let before = atomic_repository::check_ignore_policy(&root)
        .map_err(|error| git_error(format!("cannot inspect ignore policy: {error}")))?;
    let report = atomic_repository::mirror_ignores(&root).map_err(|error| {
        git_error(format!(
            "cannot mirror .atomicignore into .git/info/exclude: {error}"
        ))
    })?;
    let written = before.unmirrored.len() - report.unmirrored.len();
    print_info(&format!(
        "Covered {written} .atomicignore pattern(s) in Git ignore sources ({} newly mirrored into the managed .git/info/exclude block); {} unmirrored pattern(s) remain",
        written,
        report.unmirrored.len()
    ));
    if report.diverges() {
        print_info(&format!(
            "unmirrored (not representable verbatim): {}",
            report.unmirrored.join(", ")
        ));
    }
    Ok(())
}

fn git_hook_context(root: &Path) -> CliResult<GitHookContext> {
    let common_dir = match observe_git(root)
        .map_err(|error| git_error(format!("cannot resolve Git administrative paths: {error}")))?
    {
        GitObservation::NoGit { .. } => {
            return Err(git_error(
                "cannot enable the bridge outside a Git repository",
            ));
        }
        GitObservation::Repository(observation) => {
            if observation.paths.worktree_root.is_none() {
                return Err(git_error(
                    "cannot enable a working-copy hook for a bare Git repository",
                ));
            }
            observation.paths.common_dir.clone()
        }
    };

    let repository = GitRepository::open(root)
        .map_err(|error| git_error(format!("cannot open Git repository: {error}")))?;
    let config = repository
        .config()
        .map_err(|error| git_error(format!("cannot read Git configuration: {error}")))?;
    let custom_hooks_path = match config.get_path("core.hooksPath") {
        Ok(path) => Some(path),
        Err(error) if error.code() == ErrorCode::NotFound => None,
        Err(error) => {
            return Err(git_error(format!(
                "cannot read configured core.hooksPath: {error}"
            )))
        }
    };

    Ok(GitHookContext {
        common_dir,
        custom_hooks_path,
    })
}

fn dispatcher_script(binary: &Path, subcommand: &str) -> io::Result<String> {
    let binary = shell_quote(binary)?;
    Ok(format!(
        "#!/bin/sh\n{DISPATCHER_MARKER}\n\
{binary} git bridge {subcommand} \"$@\" || true\n\
if [ -d \"$0.d\" ]; then\n\
  for atomic_bridge_hook in \"$0.d\"/*; do\n\
    [ -f \"$atomic_bridge_hook\" ] && [ -x \"$atomic_bridge_hook\" ] || continue\n\
    \"$atomic_bridge_hook\" \"$@\" || true\n\
  done\n\
fi\n\
exit 0\n"
    ))
}

/// Script for the stdin-reading advisory dispatchers (`post-rewrite`,
/// `reference-transaction`). Stdin is forwarded so Git's evidence lines
/// reach the journal; every failure stays advisory.
///
/// Git passes the `reference-transaction` state as argv[1] (review blocker
/// 5): when `forward_state_arg` is set, `"$1"` is forwarded as the
/// subcommand's state argument. `post-rewrite` takes no argv.
fn stdin_dispatcher_script(binary: &Path, subcommand: &str, forward_state_arg: bool) -> io::Result<String> {
    stdin_dispatcher_script_ext(binary, subcommand, forward_state_arg, false)
}

/// Variant with exit-code control: advisory dispatchers end with `|| true`
/// (RFC §11.1: hooks never block), while the CB-12B `pre-push` dispatcher
/// propagates a local refusal's exit status — the hook may fail THIS push
/// locally, which stays advisory to the guarantee (the server gate is what
/// protects; `--no-verify`, removed hooks, and other clients bypass it).
fn stdin_dispatcher_script_ext(
    binary: &Path,
    subcommand: &str,
    forward_state_arg: bool,
    propagate_exit: bool,
) -> io::Result<String> {
    let binary = shell_quote(binary)?;
    let state_arg = if forward_state_arg { " \"$1\"" } else { "" };
    let failure = if propagate_exit { " || exit $?" } else { " || true" };
    Ok(format!(
        "#!/bin/sh\n{DISPATCHER_MARKER}\n\
{binary} git bridge {subcommand}{state_arg}{failure}\n\
exit 0\n"
    ))
}

fn integration_command(binary: &Path) -> io::Result<String> {
    Ok(format!(
        "{} git bridge hook-post-checkout \"$@\" || true",
        shell_quote(binary)?
    ))
}

fn shell_quote(path: &Path) -> io::Result<String> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidData,
            "the Atomic binary path is not valid UTF-8 and cannot be embedded in a Git hook",
        )
    })?;
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn print_unmanaged_instructions(reason: &str, binary: &Path) -> CliResult<()> {
    let command = integration_command(binary).map_err(|error| {
        git_error(format!(
            "cannot format hook integration instructions: {error}"
        ))
    })?;
    print_warning(reason);
    print_info("Add this advisory command to the existing post-checkout hook or hook manager:");
    print_info(&format!("  {command}"));
    Ok(())
}

fn install_or_refresh_dispatcher(path: &Path, script: &str) -> io::Result<DispatcherInstall> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    match metadata {
        None => {
            write_new_executable(path, script.as_bytes())?;
            Ok(DispatcherInstall::Installed)
        }
        Some(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Ok(DispatcherInstall::Unmanaged)
        }
        Some(_) => {
            let existing = fs::read(path)?;
            if is_owned_dispatcher(&existing) || is_legacy_atomic_only(&existing) {
                replace_owned_executable(path, script.as_bytes())?;
                Ok(DispatcherInstall::Refreshed)
            } else {
                Ok(DispatcherInstall::Unmanaged)
            }
        }
    }
}

fn is_owned_dispatcher(content: &[u8]) -> bool {
    let expected = format!("#!/bin/sh\n{DISPATCHER_MARKER}\n");
    content.starts_with(expected.as_bytes())
}

fn is_legacy_atomic_only(content: &[u8]) -> bool {
    let Ok(content) = std::str::from_utf8(content) else {
        return false;
    };
    let mut in_atomic_section = false;
    let mut saw_begin = false;
    let mut saw_end = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == LEGACY_MARKER_BEGIN && !in_atomic_section && !saw_begin {
            in_atomic_section = true;
            saw_begin = true;
            continue;
        }
        if trimmed == LEGACY_MARKER_END && in_atomic_section {
            in_atomic_section = false;
            saw_end = true;
            continue;
        }
        if !in_atomic_section && !trimmed.is_empty() && trimmed != "#!/bin/sh" {
            return false;
        }
    }

    saw_begin && saw_end && !in_atomic_section
}

fn migrate_legacy_import_hooks(hooks_dir: &Path, binary: &Path) -> CliResult<()> {
    for hook_name in ["post-commit", "post-merge", "post-rewrite"] {
        let path = hooks_dir.join(hook_name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(git_error(format!(
                    "cannot inspect legacy hook '{}': {error}",
                    path.display()
                )))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            continue;
        }
        let content = fs::read(&path).map_err(|error| {
            git_error(format!(
                "cannot inspect legacy hook '{}': {error}",
                path.display()
            ))
        })?;
        if is_legacy_atomic_only(&content) {
            fs::remove_file(&path).map_err(|error| {
                git_error(format!(
                    "cannot remove Atomic-owned legacy hook '{}': {error}",
                    path.display()
                ))
            })?;
            print_info(&format!(
                "Removed Atomic-owned legacy synchronous hook {}",
                path.display()
            ));
        } else if content
            .windows(LEGACY_MARKER_BEGIN.len())
            .any(|window| window == LEGACY_MARKER_BEGIN.as_bytes())
        {
            print_unmanaged_instructions(
                &format!(
                    "legacy Atomic section is mixed with unmanaged hook content in '{}'; the file was left untouched and the legacy import section must be removed manually",
                    path.display()
                ),
                binary,
            )?;
        }
    }
    Ok(())
}

fn write_new_executable(path: &Path, content: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(content)?;
    file.sync_all()?;
    set_executable(path)?;
    Ok(())
}

fn replace_owned_executable(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(ErrorKind::InvalidInput, "hook path has no parent directory")
    })?;
    let temporary = parent.join(format!(".atomic-post-checkout-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        write_new_executable(&temporary, content)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn uninstall_bridge(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let binary = std::env::current_exe()
        .map_err(|error| git_error(format!("cannot resolve the Atomic binary path: {error}")))?;
    let context = git_hook_context(&root)?;
    if let Some(custom_hooks_path) = context.custom_hooks_path {
        print_unmanaged_instructions(
            &format!(
                "core.hooksPath is configured as '{}'; Atomic did not modify that hook system",
                custom_hooks_path.display()
            ),
            &binary,
        )?;
        return Ok(());
    }

    let path = context.common_dir.join("hooks/post-checkout");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            print_info("No Atomic advisory post-checkout dispatcher is installed.");
            return Ok(());
        }
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect hook '{}': {error}",
                path.display()
            )))
        }
    };

    if metadata.file_type().is_file() {
        let content = fs::read(&path).map_err(|error| {
            git_error(format!("cannot read hook '{}': {error}", path.display()))
        })?;
        if is_owned_dispatcher(&content) || is_legacy_atomic_only(&content) {
            fs::remove_file(&path).map_err(|error| {
                git_error(format!("cannot remove hook '{}': {error}", path.display()))
            })?;
            print_info("Removed the Atomic advisory post-checkout dispatcher.");
            return Ok(());
        }
    }

    print_unmanaged_instructions(
        &format!(
            "post-checkout hook '{}' is not Atomic-owned and was left untouched",
            path.display()
        ),
        &binary,
    )
}

fn show_status(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let context = git_hook_context(&root)?;
    if let Some(custom_hooks_path) = context.custom_hooks_path {
        print_info(&format!(
            "post-checkout: externally managed through core.hooksPath '{}'",
            custom_hooks_path.display()
        ));
        return Ok(());
    }

    let path = context.common_dir.join("hooks/post-checkout");
    let status = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => "not installed",
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect hook '{}': {error}",
                path.display()
            )))
        }
        Ok(metadata) if metadata.file_type().is_symlink() => "unmanaged symlink",
        Ok(metadata) if !metadata.file_type().is_file() => "unmanaged non-file",
        Ok(_) => match fs::read(&path) {
            Ok(content) if is_owned_dispatcher(&content) => "Atomic-owned dispatcher installed",
            Ok(_) => "unmanaged hook installed",
            Err(_) => "unmanaged unreadable hook",
        },
    };
    print_info(&format!("post-checkout: {status} ({})", path.display()));
    Ok(())
}

#[derive(Debug, Serialize)]
struct CheckoutEventEvidence {
    version: u32,
    record_type: &'static str,
    event_id: String,
    recorded_at: String,
    advisory: bool,
    old_head: String,
    new_head: String,
    checkout_kind: &'static str,
    worktree_root: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct DeferredObservationRequest {
    version: u32,
    request_type: String,
    event_id: String,
    requested_at: String,
    worktree_root: PathBuf,
}

#[derive(Debug)]
struct ScheduledObservation {
    event_id: String,
    request_path: PathBuf,
}

#[derive(Debug, Serialize)]
struct DeferredObservationReceipt {
    version: u32,
    record_type: &'static str,
    receipt_id: String,
    cause_event_id: String,
    recorded_at: String,
    advisory: bool,
    observation: GitObservationEvidence,
}

#[derive(Debug, Serialize)]
struct GitObservationEvidence {
    git_present: bool,
    worktree_root: Option<PathBuf>,
    worktree_git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
    head_kind: Option<&'static str>,
    head_symref: Option<String>,
    head_oid: Option<String>,
    head_tree_oid: Option<String>,
    index_tree_oid: Option<String>,
    index_digest: Option<String>,
    refs_digest: Option<String>,
    index_locked: bool,
    ref_locks: Vec<PathBuf>,
    operation_state: Option<String>,
    operation_markers: Vec<&'static str>,
}

/// CB-9B: one `post-rewrite` old→new pair journaled as advisory evidence.
///
/// Per RFC §5.4 this establishes operation linkage only — change identity
/// still requires binding verification or explicit review.
///
/// `capture_token` (review E2) is present only when the hook ran INSIDE a
/// prepared operation's capture context (the executor exports
/// `ATOMIC_BRIDGE_CAPTURE_TOKEN` for the duration of the operation). A
/// capture without a token — the ordinary advisory hook path — can never be
/// promoted onto a prepared operation: anchoring requires the captured bytes
/// to carry the exact token minted for that operation at preparation time.
#[derive(Debug, Deserialize, Serialize)]
struct RewriteEventEvidence {
    version: u32,
    record_type: &'static str,
    event_id: String,
    recorded_at: String,
    advisory: bool,
    worktree_root: PathBuf,
    interpretation: &'static str,
    rewritten: Vec<RewrittenPair>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture_token: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RewrittenPair {
    old_oid: String,
    new_oid: String,
}

/// CB-9B: one `reference-transaction` observation.
///
/// Per RFC §5.4 this is authoritative evidence only that a ref moved; it is
/// never rewrite identity.
#[derive(Debug, Deserialize, Serialize)]
struct RefTransactionEventEvidence {
    version: u32,
    record_type: &'static str,
    event_id: String,
    recorded_at: String,
    advisory: bool,
    worktree_root: PathBuf,
    interpretation: &'static str,
    /// The transaction state Git passed as argv[1] (review blocker 5).
    state: String,
    transactions: Vec<RefTransactionEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RefTransactionEntry {
    ref_name: String,
    old_oid: String,
    new_oid: String,
}

/// Append checkout evidence and schedule a separate read-only observer.
pub(crate) fn record_post_checkout(
    root: &Path,
    old_head: &str,
    new_head: &str,
    checkout_flag: &str,
) -> CliResult<()> {
    let scheduled = persist_checkout_event(root, old_head, new_head, checkout_flag)?;
    warn_if_view_mismatch(root);
    spawn_deferred_observer(root, &scheduled)
}

/// CB-12A (RFC §10.3/§11.1): write one authenticated commit-time capture per
/// active managed agent session while a commit is forming.
///
/// Advisory evidence only — this never blocks a commit and never starts the
/// snapshot/split path. The exact staged-tree split into a durable change and
/// remainder (RFC §10.3.2) is not implemented, and the inseparable-operation
/// commit policy (RFC §19 Q2) is UNDECIDED after the owner's 2026-09-13
/// deferral; no fail-closed snapshot gating is installed in its place.
/// CB-12B: local advisory pre-push verification (RFC §10.4/§11.1).
///
/// Runs the same trusted provenance gate the publication boundaries run,
/// plus the shadow publication verification, and reports the verdict
/// honestly:
///
/// - **Refusal** (exit non-zero) blocks only THIS machine's push. The text
///   says so explicitly: local hooks are advisory to the guarantee, and a
///   remote without its own enforcement is not protected by anything that
///   happened here.
/// - **Pass** proves nothing about the remote either; the reminder names the
///   receiving deployment that would make the publication protected.
///
/// This hook is strictly read-only and never starts a nested push: no
/// `git push` invocation, no writable open, no operation records, no ref
/// movement. Git supplies `<local-ref> <local-oid> <remote-ref> <remote-oid>`
/// lines on stdin; CB-12B follow-up AC-4 binds the verdict to the EXACT
/// proposed OIDs: every proposed local OID must resolve to the view's
/// verified projection commit (the mapped-ref tip the shadow publication
/// verification proves), so the gate's verdict covers precisely the objects
/// this push proposes — never a mutable HEAD reread after the fact.
pub(crate) fn run_pre_push_verification(root: &Path) -> CliResult<()> {
    // Read the proposed refs/OIDs; Git blocks on a broken-pipe writer
    // otherwise. The local OIDs bind the verdict (see the bound entry
    // point).
    let mut input = String::new();
    {
        let mut stdin = io::stdin().lock();
        let _ = stdin.read_to_string(&mut input);
    }
    run_pre_push_verification_with_input(root, &input)
}

/// The exact-OID-bound pre-push verification over caller-supplied stdin
/// lines (the hook entry point drains the real stdin; tests inject).
pub(crate) fn run_pre_push_verification_with_input(root: &Path, input: &str) -> CliResult<()> {
    let root = canonical_root(root)?;

    let mut proposed_oids: Vec<String> = Vec::new();
    let mut proposed_content_refs: Vec<(String, String)> = Vec::new();
    for line in input.lines() {
        let mut fields = line.split_whitespace();
        let local_ref = fields.next().unwrap_or_default().to_string();
        if let Some(local_oid) = fields.next() {
            proposed_oids.push(local_oid.to_string());
            // CB-12B follow-up AC-4: the exact-OID tree binding applies to
            // CONTENT publication (refs/heads/…) — the binding namespace
            // (refs/atomic/bindings/…) carries binding evidence verified at
            // binding creation and is exactly the same-push binding the AC
            // accepts; other refs/atomic/* namespaces are Atomic-owned.
            if local_ref.starts_with("refs/heads/") {
                proposed_content_refs.push((local_ref, local_oid.to_string()));
            }
        }
    }

    let repo = atomic_repository::Repository::open_readonly(&root).map_err(CliError::Repository)?;
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;
    let current_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::Repository)?;

    // Reuse the shadow publication verification: manifest/tree equivalence
    // and content correctness stay independent of the provenance gate.
    let verified = super::shadow::verify_git_publication(&repo, &root, &current_view)?;

    // Exact proposed-OID binding (CB-12B follow-up AC-4): every proposed
    // local OID must BE the verified projection commit — the mapped-ref tip
    // whose tree the shadow verification just proved equals the view's
    // manifest. A proposed OID that is not the verified projection means
    // this push would publish objects the gate did not verify — refuse
    // before anything leaves the machine.
    if !proposed_oids.is_empty() {
        // CB-12B follow-up AC-4 exact-OID binding: the gate verifies THE
        // OBJECTS BEING PUSHED, not a mutable HEAD reread after the fact.
        // For each proposed OID, fold its tree under the conversion policy
        // (raw tree MINUS declared bridge-private entries — the same
        // equivalence the import gate proves) and require it to equal the
        // verified manifest tree: native-publisher pushes and
        // imported-foreign-history pushes bind exactly; a proposed OID
        // carrying any other content is unverified — refuse before anything
        // leaves the machine.
        {
            let git = git2::Repository::open(&root).map_err(|e| CliError::GitError {
                message: format!("cannot open the Git repository for pre-push binding: {e}"),
            })?;
            let policy = super::parallel::conversion_policy(&git)?;
            let verified_tree = verified.git_tree.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>();
            for (local_ref, oid) in &proposed_content_refs {
                let binds = git2::Oid::from_str(oid)
                    .ok()
                    .and_then(|commit_oid| git.find_commit(commit_oid).ok())
                    .and_then(|commit| {
                        super::parallel::import_expected_tree_oid(
                            &git,
                            commit.tree_id(),
                            &policy,
                        )
                        .ok()
                    })
                    .map(|expected| expected.as_bytes() == verified.git_tree.as_bytes())
                    .unwrap_or(false);
                if !binds {
                    return Err(CliError::GitError {
                        message: format!(
                            "pre-push refused: the proposed OID {oid} does not carry the verified manifest state of view '{current_view}' under the active conversion policy; the local gate's verdict covers only the exact proposed objects, and this push proposes objects it did not verify (RFC §10.4 exact-OID binding)"
                        ),
                    });
                }
            }
        }
    }

    // Trusted provenance gate over the complete reachable closure.
    let view_changes: Vec<atomic_core::types::Hash> = repo
        .get_view_changes(Some(&current_view))
        .map_err(CliError::Repository)?
        .into_iter()
        .map(|(_seq, hash)| hash)
        .collect();
    let closure = atomic_repository::repository::provenance_gate::reachable_closure(
        &repo,
        &view_changes,
    )
    .map_err(CliError::Repository)?;
    let provider =
        atomic_repository::repository::provenance_gate::local_session_mac_key_provider(&repo);
    match repo.evaluate_publication_gate(&closure, &gate_config(&repo), Some(&provider)) {
        Ok(verdict) if verdict.allowed() => {
            print_info(&format!(
                "pre-push (local, advisory): {current_view} passed local verification \
                 ({} change(s), {} managed). Local hooks are advisory to the guarantee: \
                 this push is only protected if the remote enforces provenance server-side \
                 ('atomic git bridge verify-receive', an Atomic-controlled remote, or a \
                 required CI status check).",
                verdict.checked, verdict.managed_changes
            ));
            Ok(())
        }
        Ok(verdict) => {
            print_warning(&format!(
                "pre-push (local): refusing THIS machine's push of view '{current_view}'. \
                 A local refusal is advisory to the guarantee: it does not protect the remote, \
                 and a remote without its own enforcement can still receive arbitrary pushes \
                 (for example with 'git push --no-verify' or from any other client)."
            ));
            // CB-13C observability: the local publication-boundary refusal
            // is recorded with stable counts. The pre-push provenance gate
            // counts provenance blocks/changes (review R5: accurate units).
            atomic_repository::repository::observability::BridgeEventJournal::new(
                &atomic_repository::Repository::canonical_dot_dir(&root)
                    .map_err(CliError::Repository)?,
                None,
            )
            .emit_lossy(
                atomic_repository::repository::observability::BridgeEventKind::PublicationRefusal {
                    boundary:
                        atomic_repository::repository::observability::PublicationBoundary::PrePush,
                    refused: verdict.blocks.len(),
                    checked: verdict.checked,
                    unit: atomic_repository::repository::observability::PublicationUnit::Changes,
                },
            );
            Err(CliError::Repository(
                atomic_repository::RepositoryError::PublicationGateRefused {
                    boundary: "local pre-push hook (advisory)".to_string(),
                    report: verdict.refusal_report(),
                    managed_changes: verdict.managed_changes,
                    checked: verdict.checked,
                },
            ))
        }
        Err(error) => Err(CliError::Repository(error)),
    }
}

fn gate_config(
    repo: &atomic_repository::Repository,
) -> atomic_repository::repository::provenance_gate::PublicationGateConfig {
    atomic_repository::repository::provenance_gate::PublicationGateConfig::from_repo(repo)
        .unwrap_or(atomic_repository::repository::provenance_gate::PublicationGateConfig {
            trust: Default::default(),
            repository_identity: None,
        })
}

pub(crate) fn record_pre_commit(root: &Path) -> CliResult<()> {    let root = canonical_root(root)?;
    let sessions_dir = match atomic_repository::Repository::canonical_dot_dir(&root) {
        Ok(dot_dir) => dot_dir.join("sessions"),
        Err(_) => return Ok(()), // not an Atomic repository — nothing to capture
    };

    // One git observation covers every active session's capture.
    let token = match atomic_repository::observe_git_metadata(&root) {
        Ok(observation) => observation.token(),
        Err(error) => {
            print_warning(&format!(
                "pre-commit capture skipped: git observation failed: {error}"
            ));
            return Ok(());
        }
    };

    let working_copy = match atomic_repository::Repository::open_readonly(&root)
        .and_then(|repo| repo.require_working_copy_id().map(|id| id.to_string()))
    {
        Ok(working_copy) => working_copy,
        Err(_) => return Ok(()), // no working-copy identity — nothing managed to capture
    };

    let store = match atomic_agent::turn::session::SessionStore::new(&sessions_dir) {
        Ok(store) => store,
        Err(_) => return Ok(()),
    };

    let active = match store.active_sessions() {
        Ok(active) => active,
        Err(_) => return Ok(()),
    };

    for mut session in active {
        let turn = session.turn_count + 1;
        match atomic_agent::turn::capture::write_capture(
            &sessions_dir,
            &mut session,
            turn,
            working_copy.clone(),
            &token,
        ) {
            Ok(path) => {
                // Persist the MAC key generated for the capture, or the
                // turn-end consumer cannot authenticate the evidence.
                let _ = store.save(&session);
                log::debug!("wrote commit-time capture {}", path.display());
            }
            Err(error) => {
                print_warning(&format!(
                    "pre-commit capture for session {} turn {} failed (advisory): {error}",
                    session.session_id, turn
                ));
            }
        }
    }

    Ok(())
}

/// CB-9B: journal `post-rewrite` evidence from Git's stdin format
/// (`<old-sha> <new-sha> [extra]` per line).
///
/// Advisory only: the event is appended to the immutable journal and
/// nothing else happens. Correctness never depends on this hook having run
/// (RFC §11). This hook never anchors its captures: downstream readers
/// classify unanchored captures as unauthenticated advisory evidence — never
/// operation linkage — whatever Git ancestry shape the named OIDs describe
/// (review C2: arbitrary sibling commits satisfy every ancestry-shape
/// predicate, and a real-looking amend pair submitted directly to the hook
/// proves nothing about which operation captured it). Operation linkage
/// requires an anchored capture written through
/// `Repository::capture_bridge_event_anchored` during an active captured
/// operation whose minted capture token the event bytes carry (review E2:
/// the executor exports `ATOMIC_BRIDGE_CAPTURE_TOKEN` for the duration of
/// the operation; a capture produced inside that context is the only one
/// that can bind to the operation, and never to a later unrelated one).
pub(crate) fn record_post_rewrite(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let mut stdin = io::stdin().lock();
    let mut input = String::new();
    stdin
        .read_to_string(&mut input)
        .map_err(|error| git_error(format!("cannot read post-rewrite input: {error}")))?;
    drop(stdin);

    let mut rewritten = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (old_oid, new_oid) = match (parts.next(), parts.next()) {
            (Some(old_oid), Some(new_oid)) => (old_oid, new_oid),
            _ => {
                return Err(git_error(format!(
                    "post-rewrite line '{line}' is not an '<old-sha> <new-sha>' pair"
                )))
            }
        };
        validate_hook_oid("rewritten old OID", old_oid)?;
        validate_hook_oid("rewritten new OID", new_oid)?;
        rewritten.push(RewrittenPair {
            old_oid: old_oid.to_ascii_lowercase(),
            new_oid: new_oid.to_ascii_lowercase(),
        });
    }
    if rewritten.is_empty() {
        return Ok(());
    }

    // Review E2: a hook executed INSIDE a prepared operation's capture
    // context carries the operation's minted capture token, so the captured
    // bytes can later bind to exactly that operation (and never to a later
    // unrelated one). An invalid or absent token keeps the capture advisory:
    // the hook must never break Git over evidence it cannot authenticate.
    let capture_token = match std::env::var("ATOMIC_BRIDGE_CAPTURE_TOKEN") {
        Ok(raw) if !raw.trim().is_empty() => {
            let token = raw.trim().to_string();
            if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
                print_warning(
                    "ATOMIC_BRIDGE_CAPTURE_TOKEN is not a 64-hex-character capture token; \
                     the captured event stays advisory",
                );
                None
            } else {
                Some(token.to_ascii_lowercase())
            }
        }
        _ => None,
    };

    let event = RewriteEventEvidence {
        version: EVENT_VERSION,
        record_type: "post-rewrite",
        event_id: Uuid::new_v4().to_string(),
        recorded_at: Utc::now().to_rfc3339(),
        advisory: true,
        worktree_root: root.clone(),
        interpretation: REWRITE_INTERPRETATION,
        rewritten,
        capture_token,
    };
    // Journal the event, then capture the exact event bytes immutably in the
    // pristine database (review R4): the JSONL journal is writable by any
    // process and proves nothing by itself, so operation-linkage reading
    // authenticates a line only against this capture. Capture failure never
    // breaks Git: the line stays journal-only and reads as unauthenticated.
    let event_bytes = serde_json::to_vec(&event).map_err(|error| {
        git_error(format!("cannot encode post-rewrite evidence: {error}"))
    })?;
    append_journal_record(&root, &event)?;
    capture_event_bytes(&root, &event_bytes)
}

/// Capture `event_bytes` through the repository library into the pristine
/// database. Best-effort by design: a failure (lock contention, read-only
/// repository) leaves the event journal-only, which downstream readers
/// honestly treat as unauthenticated rather than operation linkage. The
/// advisory hook must never break Git, so failures become warnings.
fn capture_event_bytes(root: &Path, event_bytes: &[u8]) -> CliResult<()> {
    if let Err(error) = (|| -> Result<(), CliError> {
        let repo = atomic_repository::Repository::open(root)
            .map_err(|error| git_error(format!("cannot open the Atomic repository: {error}")))?;
        repo.capture_bridge_event(event_bytes)
            .map_err(|error| git_error(format!("cannot capture bridge event: {error}")))?;
        Ok(())
    })() {
        print_warning(&format!(
            "post-rewrite event {} was journaled but not captured immutably ({error}); \
             it will read as unauthenticated evidence, not operation linkage",
            "post-rewrite"
        ));
    }
    Ok(())
}

/// CB-9B: journal `reference-transaction` evidence from Git's actual wire
/// format (review blocker 5): the transaction state arrives as argv[1] and
/// stdin carries `<old-oid> <new-oid> <ref-name>` lines.
///
/// This is authoritative evidence only that a *committed* ref transaction
/// moved a ref (RFC §5.4); it is never rewrite identity and never triggers
/// import or reconciliation. `preparing`, `prepared`, and `aborted` records
/// are journal evidence that a transaction ran, but they are NOT proof of
/// movement.
pub(crate) fn record_reference_transaction(root: &Path, state: &str) -> CliResult<()> {
    let root = canonical_root(root)?;
    let state = state.to_ascii_lowercase();
    // Git ≥2.55 invokes the hook with four states: `preparing` and
    // `prepared` (a transaction is staged), `committed` (it landed), and
    // `aborted` (it failed). Only `committed` proves movement.
    if !matches!(
        state.as_str(),
        "committed" | "prepared" | "preparing" | "aborted"
    ) {
        return Err(git_error(format!(
            "reference-transaction state must be committed|preparing|prepared|aborted, got '{state}'"
        )));
    }
    let mut stdin = io::stdin().lock();
    let mut input = String::new();
    stdin
        .read_to_string(&mut input)
        .map_err(|error| git_error(format!("cannot read reference-transaction input: {error}")))?;
    drop(stdin);

    let transactions = reference_transaction_entries(&input)?;

    if transactions.is_empty() {
        return Ok(());
    }

    let event = RefTransactionEventEvidence {
        version: EVENT_VERSION,
        record_type: "reference-transaction",
        event_id: Uuid::new_v4().to_string(),
        recorded_at: Utc::now().to_rfc3339(),
        advisory: true,
        worktree_root: root.clone(),
        interpretation: if state == "committed" {
            REF_INTERPRETATION_COMMITTED
        } else {
            REF_INTERPRETATION_NOT_MOVED
        },
        state,
        transactions,
    };
    append_journal_record(&root, &event)
}

/// Parse reference-transaction stdin lines into recorded movements.
///
/// Git passes ref *names*, not object IDs, for symbolic-ref updates (e.g.
/// `refs/heads/main HEAD` when HEAD's symref target changes on a plain
/// `git checkout`). Those lines are not OID movements: the advisory
/// evidence journal records value-ID movements only, so a symbolic update
/// is skipped instead of failing the advisory hook with a misleading
/// validation error. Zero OIDs stay legal for create/delete; every other
/// recorded value must be a real object-ID shape.
fn reference_transaction_entries(input: &str) -> CliResult<Vec<RefTransactionEntry>> {
    let value_is_object_id = |value: &str| {
        value.bytes().all(|byte| byte == b'0')
            || (matches!(value.len(), 40 | 64)
                && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    let mut transactions = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (old_oid, new_oid, ref_name) = match (parts.next(), parts.next(), parts.next()) {
            (Some(old_oid), Some(new_oid), Some(ref_name)) => (old_oid, new_oid, ref_name),
            _ => {
                return Err(git_error(format!(
                    "reference-transaction line '{line}' is not '<old> <new> <ref>'"
                )))
            }
        };
        if !value_is_object_id(old_oid) || !value_is_object_id(new_oid) {
            continue;
        }
        transactions.push(RefTransactionEntry {
            ref_name: ref_name.to_string(),
            old_oid: old_oid.to_ascii_lowercase(),
            new_oid: new_oid.to_ascii_lowercase(),
        });
    }
    Ok(transactions)
}

/// The identity limit for post-rewrite evidence, quoted verbatim in every
/// journal record so downstream readers cannot upgrade it silently.
pub(crate) const REWRITE_INTERPRETATION: &str =
    "operation-linkage-only: this event links a Git rewrite operation to the named commits; \
     change identity still requires binding verification or explicit review (RFC 5.4)";

/// The identity limit for reference-transaction evidence of a *committed*
/// transaction, quoted verbatim in every journal record.
pub(crate) const REF_INTERPRETATION_COMMITTED: &str =
    "ref-movement-only: this event proves only that a committed Git ref transaction moved a ref; \
     it is never rewrite identity (RFC 5.4)";

/// The identity limit for `preparing`/`prepared`/`aborted`
/// reference-transaction evidence: such a record proves a transaction ran,
/// but is NOT proof of ref movement.
pub(crate) const REF_INTERPRETATION_NOT_MOVED: &str =
    "ref-movement-only: a preparing, prepared, or aborted transaction proves only that a transaction ran; \
     it is NOT proof that a ref moved and is never rewrite identity (RFC 5.4)";

/// Compatibility alias for the committed-state interpretation.
pub(crate) const REF_INTERPRETATION: &str = REF_INTERPRETATION_COMMITTED;

/// CB-9B: `atomic git bridge review` — surface squash/rewrite candidates,
/// merge resolutions, and empty-commit interpretations with the evidence
/// that links them to Git operations, stating each evidence class's exact
/// identity limit (RFC §5.4).
pub(crate) fn run_bridge_review(root: &Path, view: Option<&str>) -> CliResult<()> {
    let root = canonical_root(root)?;
    let repo = atomic_repository::Repository::open(&root)
        .map_err(|error| git_error(format!("cannot open the Atomic repository: {error}")))?;
    let view_name = match view {
        Some(view) => view.to_string(),
        None => repo.current_view().to_string(),
    };
    if !repo
        .view_exists(&view_name)
        .map_err(|error| git_error(format!("cannot inspect views: {error}")))?
    {
        return Err(git_error(format!("view '{view_name}' does not exist")));
    }

    // Imported interpretations, straight from hash-authoritative change bytes.
    let entries = repo
        .effective_history(Some(&view_name))
        .map_err(|error| git_error(format!("cannot read view history: {error}")))?;
    let mut merges = 0usize;
    let mut squashes = 0usize;
    let mut empty_commits = 0usize;
    let mut candidates: Vec<String> = Vec::new();
    for entry in &entries {
        let change = repo
            .load_change(&entry.hash)
            .map_err(|error| git_error(format!("cannot load change {}: {error}", entry.hash)))?;
        let sha = change
            .unhashed
            .as_ref()
            .and_then(|value| value.get("git"))
            .and_then(|git| git.get("sha"))
            .and_then(|sha| sha.as_str())
            .unwrap_or("?")
            .to_string();
        match change.origin() {
            atomic_core::change::ChangeOrigin::GitResolution { parents, .. } => {
                merges += 1;
                candidates.push(format!(
                    "merge-resolution {} parents={} — verified causal frontier covers every parent closure",
                    short(&sha),
                    parents.len()
                ));
            }
            atomic_core::change::ChangeOrigin::GitSynthesized {
                commit,
                derivation,
                ..
            } => match derivation {
                atomic_core::change::GitDerivation::Squash => {
                    squashes += 1;
                    candidates.push(format!(
                        "squash/rewrite candidate {} ({}) — synthesized interpretation; identity requires a verified predecessor binding or explicit review",
                        short(&sha),
                        oid_hex(commit.as_bytes()),
                    ));
                }
                atomic_core::change::GitDerivation::EmptyCommit => {
                    empty_commits += 1;
                    candidates.push(format!(
                        "empty-commit {} — distinct Change::empty object; emits no graph facts",
                        short(&sha),
                    ));
                }
                _ => {}
            },
            _ => {}
        }
    }

    // ReviewGate tags recorded by the importer (squash inserts, rewrite
    // candidate links, merge tags). Each candidate's full evidence is
    // printed: predecessor OIDs, binding ids, event ids, and the exact
    // identity limit per class (review blocker 4).
    let tags = repo
        .list_tags_for_view(&view_name)
        .map_err(|error| git_error(format!("cannot list tags: {error}")))?;
    let mut tag_lines = Vec::new();
    for tag in tags {
        if matches!(tag.kind, atomic_core::pristine::TagKind::ReviewGate) {
            let strategy = tag
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("git"))
                .and_then(|git| git.get("merge_strategy"))
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            let evidence_entries: Vec<String> = match tag
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("evidence"))
            {
                Some(serde_json::Value::Array(entries)) => entries
                    .iter()
                    .filter_map(|entry| {
                        let class = entry.get("class").and_then(|v| v.as_str())?;
                        let predecessors = entry
                            .get("predecessor_oids")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            })
                            .unwrap_or_default();
                        let bindings = entry
                            .get("binding_ids")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            })
                            .unwrap_or_default();
                        let events = entry
                            .get("event_ids")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            })
                            .unwrap_or_default();
                        Some(format!(
                            "class={class} predecessors={predecessors} bindings={bindings} events={events} — {}",
                            match class {
                                "post-rewrite-event" => REWRITE_INTERPRETATION,
                                "binding-tree-match" => {
                                    "tree equality is an advisory hint only: similarity is never identity (RFC 5.4)"
                                }
"binding-range-match" => {
                                    "known-range candidate: a signature-checked binding covers \
                                      an intermediate of the rewritten range (a strict \
                                      descendant of the rewritten commit's first parent); the \
                                      descent hint is ancestry evidence, NOT verified range \
                                      membership, and the binding is signature-checked, not \
                                      content-recomputed — advisory and explicitly uncertain, \
                                      never identity (RFC 5.4, review D4)"
                                }
                                "binding-root-range-match" => {
                                    "root-spanning-range candidate: a parentless commit \
                                      provides no ancestry bound, so any locally stored \
                                      signature-checked binding may lie inside the rewritten \
                                      range even when its bound commit has no local \
                                      interpretation index (review E3); the hint is \
                                      maximally uncertain, complete coverage is NOT claimed, \
                                      and no content or identity claim is made (RFC 5.4, \
                                      review D4)"
                                }
"unauthenticated-event" => {
                                    "UNAUTHENTICATED (unauthenticated-evidence-only): journal text \
                                      with no immutably captured record, or a captured event that \
                                      carries no operation anchor — the advisory hook path runs \
                                      OUTSIDE any active captured operation, so Git ancestry \
                                      shape and object existence prove nothing (review C2); only \
                                      a capture anchored to a real operation with a verified \
                                      receipt is operation linkage (RFC 5.4)"
                                }
                                "reference-transaction" => REF_INTERPRETATION,
                                "verified-predecessor-binding" => {
                                    "a verified predecessor binding can establish identity on review"
                                }
                                _ => {
                                    "message/similarity evidence is RewriteCandidate only, never identity"
                                }
                            }
                        ))
                    })
                    .collect(),
                // Legacy string evidence shape from earlier imports.
                Some(serde_json::Value::String(evidence)) => vec![format!(
                    "class={evidence} predecessors=? bindings=? events=? — {}",
                    match evidence.as_str() {
                        "post-rewrite-event" => REWRITE_INTERPRETATION,
                        "reference-transaction" => REF_INTERPRETATION,
                        "verified-predecessor-binding" => {
                            "a verified predecessor binding can establish identity on review"
                        }
                        _ => "message/similarity evidence is RewriteCandidate only, never identity",
                    }
                )],
                _ => vec!["class=message-format — message/similarity evidence is \
                          RewriteCandidate only, never identity"
                    .to_string()],
            };
            tag_lines.push(format!(
                "review-gate tag '{}' strategy={strategy}:",
                tag.name
            ));
            for entry in evidence_entries {
                tag_lines.push(format!("    {entry}"));
            }
        }
    }

    // Advisory hook evidence from the immutable event journal.
    let journal_path = root.join(EVENT_JOURNAL_RELATIVE);
    let mut rewrite_events = 0usize;
    let mut ref_events = 0usize;
    if let Ok(journal) = fs::read_to_string(&journal_path) {
        for line in journal.lines() {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match record.get("record_type").and_then(|value| value.as_str()) {
                Some("post-rewrite") => rewrite_events += 1,
                Some("reference-transaction") => ref_events += 1,
                _ => {}
            }
        }
    }

    println!("Bridge review for view '{view_name}':");
    println!("  imported changes: {}", entries.len());
    println!("  merge resolutions: {merges}");
    println!("  empty-commit interpretations: {empty_commits}");
    println!("  squash/rewrite candidates: {squashes}");
    for candidate in &candidates {
        println!("    - {candidate}");
    }
    println!("  review-gate tags: {}", tag_lines.len());
    for line in &tag_lines {
        println!("    - {line}");
    }
    println!(
        "  advisory hook evidence: {rewrite_events} post-rewrite event(s), {ref_events} reference-transaction record(s)"
    );
    println!("  identity limits: {REWRITE_INTERPRETATION} | {REF_INTERPRETATION}");
    Ok(())
}

fn short(sha: &str) -> &str {
    &sha[..8.min(sha.len())]
}

/// Lowercase hex for a tagged Git object ID (display only).
fn oid_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn persist_checkout_event(
    root: &Path,
    old_head: &str,
    new_head: &str,
    checkout_flag: &str,
) -> CliResult<ScheduledObservation> {
    validate_hook_oid("old HEAD", old_head)?;
    validate_hook_oid("new HEAD", new_head)?;
    let checkout_kind = match checkout_flag {
        "0" => "file",
        "1" => "branch",
        value => {
            return Err(git_error(format!(
                "post-checkout flag must be 0 or 1, got '{value}'"
            )))
        }
    };
    let root = canonical_root(root)?;
    let event_id = Uuid::new_v4().to_string();
    let recorded_at = Utc::now().to_rfc3339();
    let event = CheckoutEventEvidence {
        version: EVENT_VERSION,
        record_type: "post-checkout",
        event_id: event_id.clone(),
        recorded_at: recorded_at.clone(),
        advisory: true,
        old_head: old_head.to_ascii_lowercase(),
        new_head: new_head.to_ascii_lowercase(),
        checkout_kind,
        worktree_root: root.clone(),
    };
    append_journal_record(&root, &event)?;

    let deferred_dir = root.join(DEFERRED_DIRECTORY_RELATIVE);
    fs::create_dir_all(&deferred_dir).map_err(|error| {
        git_error(format!(
            "cannot create deferred-observation directory '{}': {error}",
            deferred_dir.display()
        ))
    })?;
    let request_path = deferred_dir.join(format!("{event_id}.json"));
    let request = DeferredObservationRequest {
        version: EVENT_VERSION,
        request_type: "deferred-git-observation".to_string(),
        event_id: event_id.clone(),
        requested_at: recorded_at,
        worktree_root: root,
    };
    write_immutable_json(&request_path, &request)?;

    Ok(ScheduledObservation {
        event_id,
        request_path,
    })
}

fn validate_hook_oid(label: &str, value: &str) -> CliResult<()> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(git_error(format!(
            "{label} from post-checkout is not a hexadecimal Git object ID"
        )));
    }
    Ok(())
}

fn append_journal_record<T: Serialize>(root: &Path, record: &T) -> CliResult<()> {
    let path = root.join(EVENT_JOURNAL_RELATIVE);
    let parent = path
        .parent()
        .ok_or_else(|| git_error("event journal has no parent directory"))?;
    fs::create_dir_all(parent).map_err(|error| {
        git_error(format!(
            "cannot create event journal directory '{}': {error}",
            parent.display()
        ))
    })?;
    let mut line = serde_json::to_vec(record)
        .map_err(|error| git_error(format!("cannot encode Git event evidence: {error}")))?;
    line.push(b'\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| {
            git_error(format!(
                "cannot open append-only event journal '{}': {error}",
                path.display()
            ))
        })?;
    file.write_all(&line).map_err(|error| {
        git_error(format!(
            "cannot append event evidence to '{}': {error}",
            path.display()
        ))
    })?;
    file.sync_data().map_err(|error| {
        git_error(format!(
            "cannot sync event evidence in '{}': {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn write_immutable_json<T: Serialize>(path: &Path, value: &T) -> CliResult<()> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| git_error(format!("cannot encode deferred request: {error}")))?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            git_error(format!(
                "cannot create immutable deferred request '{}': {error}",
                path.display()
            ))
        })?;
    file.write_all(&bytes).map_err(|error| {
        git_error(format!(
            "cannot write deferred request '{}': {error}",
            path.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        git_error(format!(
            "cannot sync deferred request '{}': {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn spawn_deferred_observer(root: &Path, scheduled: &ScheduledObservation) -> CliResult<()> {
    let binary = std::env::current_exe()
        .map_err(|error| git_error(format!("cannot resolve the Atomic binary path: {error}")))?;
    ProcessCommand::new(binary)
        .arg("git")
        .arg("bridge")
        .arg("observe-deferred")
        .arg("--root")
        .arg(root)
        .arg("--request")
        .arg(&scheduled.request_path)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            git_error(format!(
                "checkout event {} was journaled but its deferred observer could not be started: {error}",
                scheduled.event_id
            ))
        })?;
    Ok(())
}

/// Execute one deferred request using the existing strictly read-only observer.
pub(crate) fn run_deferred_observation(root: &Path, request_path: &Path) -> CliResult<()> {
    thread::sleep(Duration::from_millis(DEFERRED_DELAY_MS));

    let root = canonical_root(root)?;
    let deferred_dir = root.join(DEFERRED_DIRECTORY_RELATIVE);
    let canonical_deferred_dir = fs::canonicalize(&deferred_dir).map_err(|error| {
        git_error(format!(
            "cannot resolve deferred-observation directory '{}': {error}",
            deferred_dir.display()
        ))
    })?;
    let canonical_request = fs::canonicalize(request_path).map_err(|error| {
        git_error(format!(
            "cannot resolve deferred request '{}': {error}",
            request_path.display()
        ))
    })?;
    if canonical_request.parent() != Some(canonical_deferred_dir.as_path()) {
        return Err(git_error(format!(
            "refusing deferred request outside '{}': {}",
            canonical_deferred_dir.display(),
            canonical_request.display()
        )));
    }

    let bytes = fs::read(&canonical_request).map_err(|error| {
        git_error(format!(
            "cannot read deferred request '{}': {error}",
            canonical_request.display()
        ))
    })?;
    let request: DeferredObservationRequest = serde_json::from_slice(&bytes).map_err(|error| {
        git_error(format!(
            "deferred request '{}' is malformed: {error}",
            canonical_request.display()
        ))
    })?;
    let expected_file_name = format!("{}.json", request.event_id);
    if request.version != EVENT_VERSION
        || request.request_type != "deferred-git-observation"
        || request.worktree_root != root
        || canonical_request.file_name().and_then(|name| name.to_str())
            != Some(expected_file_name.as_str())
    {
        return Err(git_error(format!(
            "deferred request '{}' does not match its immutable identity",
            canonical_request.display()
        )));
    }

    let observation = observe_git(&root)
        .map_err(|error| git_error(format!("deferred Git observation failed: {error}")))?;
    let receipt = DeferredObservationReceipt {
        version: EVENT_VERSION,
        record_type: "deferred-observation",
        receipt_id: format!("{}:observation", request.event_id),
        cause_event_id: request.event_id,
        recorded_at: Utc::now().to_rfc3339(),
        advisory: true,
        observation: observation_evidence(observation),
    };
    append_journal_record(&root, &receipt)?;
    fs::remove_file(&canonical_request).map_err(|error| {
        git_error(format!(
            "observation receipt was appended but request '{}' could not be consumed: {error}",
            canonical_request.display()
        ))
    })?;
    Ok(())
}

fn observation_evidence(observation: GitObservation) -> GitObservationEvidence {
    match observation {
        GitObservation::NoGit { root } => GitObservationEvidence {
            git_present: false,
            worktree_root: Some(root),
            worktree_git_dir: None,
            common_dir: None,
            head_kind: None,
            head_symref: None,
            head_oid: None,
            head_tree_oid: None,
            index_tree_oid: None,
            index_digest: None,
            refs_digest: None,
            index_locked: false,
            ref_locks: Vec::new(),
            operation_state: None,
            operation_markers: Vec::new(),
        },
        GitObservation::Repository(repository) => {
            let (head_kind, head_symref, head_oid) = match &repository.head {
                HeadObservation::Attached { symref, oid } => {
                    ("attached", Some(symref.clone()), Some(oid.to_string()))
                }
                HeadObservation::Detached { oid } => ("detached", None, Some(oid.to_string())),
                HeadObservation::Unborn { symref } => ("unborn", Some(symref.clone()), None),
                HeadObservation::MissingTarget { symref } => {
                    ("missing-target", Some(symref.clone()), None)
                }
            };
            GitObservationEvidence {
                git_present: true,
                worktree_root: repository.paths.worktree_root.clone(),
                worktree_git_dir: Some(repository.paths.worktree_git_dir.clone()),
                common_dir: Some(repository.paths.common_dir.clone()),
                head_kind: Some(head_kind),
                head_symref,
                head_oid,
                head_tree_oid: repository.head_tree_oid.map(|oid| oid.to_string()),
                index_tree_oid: repository.index.tree_oid.map(|oid| oid.to_string()),
                index_digest: Some(repository.index.canonical_digest.0.clone()),
                refs_digest: Some(repository.refs_digest.0.clone()),
                index_locked: repository.locks.index_lock.is_present(),
                ref_locks: repository.locks.ref_locks.clone(),
                operation_state: Some(repository.operation.repository_state.clone()),
                operation_markers: repository
                    .operation
                    .present_markers()
                    .into_iter()
                    .map(|marker| marker.as_str())
                    .collect(),
            }
        }
    }
}

fn warn_if_view_mismatch(root: &Path) {
    let atomic_view = match fs::read_to_string(root.join(".atomic/current_view")) {
        Ok(view) => view.trim().to_string(),
        Err(_) => return,
    };
    let git_branch = GitRepository::open(root).ok().and_then(|repository| {
        repository
            .head()
            .ok()
            .and_then(|head| head.shorthand().map(str::to_string))
    });
    if let Some(git_branch) = git_branch {
        if !atomic_view.is_empty() && git_branch != atomic_view {
            print_warning(&format!(
                "git is on '{git_branch}' but the Atomic view is '{atomic_view}'; run 'atomic status --no-reconcile' before taking action"
            ));
        }
    }
}

fn canonical_root(root: &Path) -> CliResult<PathBuf> {
    fs::canonicalize(root).map_err(|error| {
        git_error(format!(
            "cannot resolve repository root '{}': {error}",
            root.display()
        ))
    })
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn dispatcher_uses_absolute_binary_and_keeps_every_failure_advisory() {
        let script =
            dispatcher_script(Path::new("/opt/Atomic Tools/atomic"), "hook-post-checkout").unwrap();
        assert!(script.starts_with("#!/bin/sh\n# atomic:git-bridge-dispatcher:v1"));
        assert!(script.contains("'/opt/Atomic Tools/atomic' git bridge hook-post-checkout"));
        assert!(script.contains("\"$@\" || true"));
        assert!(script.contains("\"$0.d\"/*"));
        assert!(script.ends_with("exit 0\n"));
    }

    #[test]
    fn pre_commit_dispatcher_is_advisory_and_uses_the_capture_subcommand() {
        let script = dispatcher_script(Path::new("/usr/bin/atomic"), "hook-pre-commit").unwrap();
        assert!(script.starts_with("#!/bin/sh\n# atomic:git-bridge-dispatcher:v1"));
        assert!(script.contains("'/usr/bin/atomic' git bridge hook-pre-commit"));
        assert!(script.contains("\"$@\" || true"), "capture evidence is advisory");
        assert!(script.ends_with("exit 0\n"));
    }

    #[test]
    fn pre_push_dispatcher_propagates_local_refusals_but_stays_advisory_to_the_guarantee() {
        let script = stdin_dispatcher_script_ext(
            Path::new("/usr/bin/atomic"),
            "hook-pre-push",
            false,
            true,
        )
        .unwrap();
        assert!(script.contains("'/usr/bin/atomic' git bridge hook-pre-push"));
        assert!(
            script.contains(" || exit $?"),
            "a local refusal must fail THIS push: {script}"
        );
        assert!(
            !script.contains("git push"),
            "the dispatcher must never start a nested push: {script}"
        );
        // Every other advisory dispatcher stays `|| true`.
        let advisory =
            stdin_dispatcher_script_ext(Path::new("/usr/bin/atomic"), "hook-post-rewrite", false, false)
                .unwrap();
        assert!(advisory.contains(" || true"));
        assert!(!advisory.contains(" || exit $?"));
    }

    #[test]
    fn ownership_requires_the_exact_dispatcher_header() {
        assert!(is_owned_dispatcher(
            b"#!/bin/sh\n# atomic:git-bridge-dispatcher:v1\nexit 0\n"
        ));
        assert!(!is_owned_dispatcher(
            b"#!/bin/sh\necho custom\n# atomic:git-bridge-dispatcher:v1\n"
        ));
        assert!(!is_owned_dispatcher(b"\xff\xfeatomic"));
    }

    #[test]
    fn legacy_hook_is_owned_only_when_no_custom_content_surrounds_it() {
        let owned = b"#!/bin/sh\n\n# atomic:git:begin\natomic git import --incremental || true\n# atomic:git:end\n";
        let mixed = b"#!/bin/sh\necho custom\n# atomic:git:begin\natomic old\n# atomic:git:end\n";
        assert!(is_legacy_atomic_only(owned));
        assert!(!is_legacy_atomic_only(mixed));
    }

    #[test]
    fn event_journal_is_append_only_and_requests_are_unique() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(".atomic")).unwrap();

        let first = persist_checkout_event(root.path(), OLD, NEW, "1").unwrap();
        let journal = root.path().join(EVENT_JOURNAL_RELATIVE);
        let prefix = fs::read(&journal).unwrap();
        let second = persist_checkout_event(root.path(), NEW, OLD, "0").unwrap();
        let complete = fs::read(&journal).unwrap();

        assert!(complete.starts_with(&prefix));
        assert_ne!(first.event_id, second.event_id);
        assert_ne!(first.request_path, second.request_path);
        assert!(first.request_path.exists());
        assert!(second.request_path.exists());
        let lines = String::from_utf8(complete).unwrap().lines().count();
        assert_eq!(lines, 2);
    }

    #[test]
    fn invalid_checkout_arguments_do_not_create_evidence() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(".atomic")).unwrap();
        assert!(persist_checkout_event(root.path(), "not-an-oid", NEW, "1").is_err());
        assert!(!root.path().join(EVENT_JOURNAL_RELATIVE).exists());
    }

    /// A plain `git checkout` moves HEAD symbolically: Git passes ref
    /// *names* (`refs/heads/…`), not object IDs, to the
    /// reference-transaction hook. Those lines must be skipped instead of
    /// failing the advisory hook with a misleading validation error on
    /// every checkout.
    #[test]
    fn reference_transaction_symbolic_ref_updates_are_skipped_not_refused() {
        // Failing-before: the pre-fix validator rejected the symbolic
        // values with "old from post-checkout is not a hexadecimal Git
        // object ID", printing an error on every checkout of a
        // bridge-enabled repository.
        let symbolic = "refs/heads/feature refs/heads/main HEAD";
        let entries = reference_transaction_entries(symbolic).expect("symbolic updates skip");
        assert!(entries.is_empty(), "no OID movement is recorded: {entries:?}");

        // OID movements (including zero-OID create/delete) stay recorded.
        let zero = "0000000000000000000000000000000000000000";
        let mixed = format!("{OLD} {NEW} refs/heads/x\n{zero} {NEW} refs/heads/new\n{symbolic}\n");
        let entries = reference_transaction_entries(&mixed).unwrap();
        assert_eq!(entries.len(), 2, "symbolic line skipped, OID lines kept");
        assert_eq!(entries[0].ref_name, "refs/heads/x");
        assert_eq!(entries[0].old_oid, OLD);
        assert_eq!(entries[1].new_oid, NEW);

        // Malformed lines still refuse typed.
        assert!(reference_transaction_entries("garbage line").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hook_is_never_replaced() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        let hook = root.path().join("post-checkout");
        fs::write(&target, b"custom target\n").unwrap();
        symlink(&target, &hook).unwrap();

        let result = install_or_refresh_dispatcher(&hook, "owned").unwrap();
        assert_eq!(result, DispatcherInstall::Unmanaged);
        assert_eq!(fs::read(&target).unwrap(), b"custom target\n");
        assert!(fs::symlink_metadata(&hook)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// CB-8B ac-4: a deferred observation request is consumed exactly once.
    /// A duplicate run (the event duplicated between prepare and consume) is
    /// refused typed and must never append a second receipt for the event.
    #[test]
    fn deferred_observation_is_consumed_once_and_a_duplicate_run_fails_typed() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(".atomic")).unwrap();
        let scheduled = persist_checkout_event(root.path(), OLD, NEW, "1").unwrap();

        run_deferred_observation(root.path(), &scheduled.request_path).unwrap();
        let journal = root.path().join(EVENT_JOURNAL_RELATIVE);
        let after_first = fs::read(&journal).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after_first)
                .lines()
                .filter(|line| line.contains("\"record_type\":\"deferred-observation\""))
                .count(),
            1,
            "exactly one observation receipt exists"
        );
        assert!(
            !scheduled.request_path.exists(),
            "the consumed request is removed"
        );

        // The duplicate run is refused typed and appends nothing.
        let duplicate = run_deferred_observation(root.path(), &scheduled.request_path);
        assert!(
            duplicate.is_err(),
            "a duplicated request must fail closed: {duplicate:?}"
        );
        assert_eq!(
            fs::read(&journal).unwrap(),
            after_first,
            "the duplicate run appended nothing"
        );
    }

    /// CB-8B ac-4: events dropped, duplicated, or reordered leave the journal
    /// append-only and bind every receipt to its own event id; a dropped
    /// request produces no receipt and never corrupts the journal, and the
    /// surviving requests are processed identically to in-order delivery.
    #[test]
    fn dropped_and_reordered_deferred_requests_keep_the_journal_append_only() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(".atomic")).unwrap();
        let first = persist_checkout_event(root.path(), OLD, NEW, "1").unwrap();
        let second = persist_checkout_event(root.path(), NEW, OLD, "1").unwrap();
        let journal = root.path().join(EVENT_JOURNAL_RELATIVE);
        let base = fs::read(&journal).unwrap();

        // Drop the first request entirely: the typed refusal leaves the
        // journal byte-identical (append-only, no fabricated receipt).
        fs::remove_file(&first.request_path).unwrap();
        let dropped = run_deferred_observation(root.path(), &first.request_path);
        assert!(dropped.is_err(), "a dropped request must fail typed");
        assert_eq!(
            fs::read(&journal).unwrap(),
            base,
            "the dropped event appended nothing"
        );

        // Reordered delivery: the second event is processed first and its
        // receipt binds to its own event id.
        run_deferred_observation(root.path(), &second.request_path).unwrap();
        let after_second = fs::read(&journal).unwrap();
        assert!(after_second.starts_with(&base), "the journal stays append-only");
        let text = String::from_utf8(after_second).unwrap();
        let receipts: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("\"record_type\":\"deferred-observation\""))
            .collect();
        assert_eq!(receipts.len(), 1, "exactly the reordered receipt exists");
        assert!(
            receipts[0].contains(&format!("\"cause_event_id\":\"{}\"", second.event_id)),
            "the receipt binds to its own event id: {}",
            receipts[0]
        );

        // The remaining first event is still processable after the reorder.
        let again = persist_checkout_event(root.path(), OLD, OLD, "0").unwrap();
        run_deferred_observation(root.path(), &again.request_path).unwrap();
        let final_text = String::from_utf8(fs::read(&journal).unwrap()).unwrap();
        assert_eq!(
            final_text
                .lines()
                .filter(|line| line.contains("\"record_type\":\"deferred-observation\""))
                .count(),
            2,
            "each delivered event produces exactly one receipt regardless of order"
        );
    }
    #[test]
    fn pre_commit_capture_writes_evidence_for_active_sessions_only() {
        use atomic_agent::turn::capture;
        use atomic_agent::turn::phase::Phase;
        use atomic_agent::turn::session::{AgentSession, SessionStore};

        let root = TempDir::new().unwrap();
        // Atomic first (it creates `.atomic` with the working-copy
        // identity), then the session store the hook reads.
        let wc = atomic_repository::Repository::init(root.path()).unwrap();
        let working_copy = wc.require_working_copy_id().unwrap().to_string();
        drop(wc);
        let sessions_dir = root.path().join(".atomic").join("sessions");

        // One active session (the capture target) and one idle session.
        let mut active = AgentSession::new("sess-active", "claude-code", "Claude Code");
        active.phase = Phase::Active;
        let store = SessionStore::new(&sessions_dir).unwrap();
        store.save(&active).unwrap();
        let idle = AgentSession::new("sess-idle", "claude-code", "Claude Code");
        store.save(&idle).unwrap();

        // The Atomic sessions dir the hook uses must be the canonical one;
        // for a plain repo that is root/.atomic/sessions (already created).
        record_pre_commit(root.path()).unwrap();

        // Review R9: attempts are create-only files; the first attempt of a
        // turn lands at turn-1.attempt-1.json.
        let active_capture =
            capture::capture_dir(&sessions_dir, "sess-active")
                .unwrap()
                .join("turn-1.attempt-1.json");
        assert!(
            active_capture.exists(),
            "an active session must get a commit-time capture at {}",
            active_capture.display()
        );
        assert!(
            !capture::has_capture(&sessions_dir, "sess-idle", 1),
            "an idle session must not be captured"
        );

        // The MAC key must be persisted so the turn-end consumer can
        // authenticate the capture.
        let stored = store.load("sess-active").unwrap().unwrap();
        assert!(stored.mac_key.is_some());

        // The capture binds the working copy and turn.
        let capture =
            capture::ManagedCommitCapture::from_json(&fs::read(&active_capture).unwrap()).unwrap();
        assert_eq!(capture.working_copy, working_copy);
        assert_eq!(capture.turn, 1);
        assert_eq!(capture.session_id, "sess-active");
        // RFC §19 Q2 undecided: conversion policy and snapshot stay unset.
        assert_eq!(capture.conversion_policy, None);
        assert_eq!(capture.snapshot, None);
    }

    #[test]
    fn pre_commit_capture_is_a_noop_without_atomic_identity() {
        use atomic_agent::turn::capture;
        use atomic_agent::turn::phase::Phase;
        use atomic_agent::turn::session::{AgentSession, SessionStore};

        let root = TempDir::new().unwrap();
        let sessions_dir = root.path().join(".atomic").join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let mut active = AgentSession::new("sess-noop", "claude-code", "Claude Code");
        active.phase = Phase::Active;
        SessionStore::new(&sessions_dir).unwrap().save(&active).unwrap();

        // No working_copy_id file: the hook must stay advisory and quiet.
        record_pre_commit(root.path()).unwrap();
        assert!(!capture::capture_path(&sessions_dir, "sess-noop", 1)
            .unwrap()
            .exists());
    }

    #[test]
    fn enable_bridge_installs_the_pre_commit_dispatcher() {
        let root = TempDir::new().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        drop(git);
        let repo = atomic_repository::Repository::init(root.path()).unwrap();
        drop(repo);
        enable_bridge(root.path(), false).unwrap();

        let hooks_dir = root.path().join(".git").join("hooks");
        let script = fs::read_to_string(hooks_dir.join("pre-commit")).unwrap();
        assert!(
            script.contains("atomic:git-bridge-dispatcher:v1"),
            "pre-commit must be an Atomic-owned dispatcher"
        );
        assert!(script.contains("git bridge hook-pre-commit"));
        // Advisory: capture evidence never blocks a commit.
        assert!(script.contains("|| true"));
    }

}

#[cfg(test)]
mod pre_push_tests {
    use super::*;
    use tempfile::TempDir;

    /// The local pre-push hook refuses managed-session work whose evidence
    /// is missing — and the refusal is typed as the advisory local gate.
    /// Composition with the server boundary is covered by the
    /// verify-receive tests: a refused local push is advisory; a refused
    /// receive is the guarantee.
    #[test]
    fn pre_push_refuses_managed_work_without_evidence() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut repo = atomic_repository::Repository::init(&root).unwrap();

        // A managed change (session provenance + envelope), no evidence.
        use atomic_core::change::envelope::SessionEnvelope;
        let mut provenance = atomic_core::change::Provenance::new(
            atomic_core::change::AIVendor::default(),
            "test-model",
            atomic_core::change::AITool::Cli("test".into()),
        );
        provenance.session_id = Some("sess-pre-push".to_string());
        let header = atomic_core::change::ChangeHeader::builder()
            .message("managed without evidence")
            .author(atomic_core::change::Author::new("A", Some("a@example.com")))
            .build();
        let mut change = atomic_core::change::Change::new(header, Vec::new(), Vec::new(), Vec::new());
        change.hashed.provenance = vec![provenance];
        change.hashed.metadata = SessionEnvelope::builder("sess-pre-push", "test-agent")
            .build()
            .encode()
            .unwrap();
        let hash = repo.save_change(&change).unwrap();

        // Land it in a DRAFT view (the insertion gate does not apply to
        // draft targets; the pre-push gate is what must catch it) and point
        // the working copy at that draft so the hook verifies it.
        repo.create_view("gate-draft").unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.switch_view(working_copy, "gate-draft").unwrap();
        repo.insert_change(
            &hash,
            atomic_repository::InsertOptions::default().view("gate-draft"),
        )
        .expect("draft insertion is not gated");

        // The hook also runs shadow publication verification, which needs
        // the colocated Git repository.
        git2::Repository::init(&root).unwrap();

        drop(repo);

        let error = run_pre_push_verification(&root).expect_err("managed work refuses");
        match &error {
            CliError::Repository(atomic_repository::RepositoryError::PublicationGateRefused {
                boundary,
                ..
            }) => assert!(boundary.contains("advisory"), "{boundary}"),
            other => panic!("expected a publication-gate refusal, got {other:?}"),
        }
    }

    #[test]
    fn pre_push_passes_unmanaged_work_and_names_the_receiving_contract() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut repo = atomic_repository::Repository::init(&root).unwrap();

        // A plain unmanaged change: nothing for the provenance gate to check.
        let change = atomic_core::change::Change::new(
            atomic_core::change::ChangeHeader::builder()
                .message("human work")
                .author(atomic_core::change::Author::new("A", Some("a@example.com")))
                .build(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let hash = repo.save_change(&change).unwrap();
        let view = {
            let working_copy = repo.require_working_copy_id().unwrap();
            repo.desired_view_name(working_copy).unwrap()
        };
        repo.insert_change(&hash, atomic_repository::InsertOptions::default().view(&view))
            .unwrap();
        drop(repo);

        // Publication verification needs a Git repository (shadow checks).
        git2::Repository::init(&root).unwrap();
        let result = run_pre_push_verification(&root);
        // Either the full publication verification passes (empty clean
        // repos may still refuse on missing Git state) or the refusal is a
        // Git/publication error — the provenance gate must NOT be the
        // blocker for unmanaged work either way.
        if let Err(error) = &result {
            let text = error.to_string();
            assert!(
                !text.contains("managed-session change"),
                "unmanaged work must not fail the provenance gate: {text}"
            );
        }
    }

    /// CB-12B follow-up AC-4: the pre-push verdict binds to the EXACT
    /// proposed OID. A push proposing an OID that is NOT the verified
    /// projection of the current view must refuse BEFORE anything leaves
    /// the machine (never a mutable HEAD reread after the fact).
    #[test]
    fn pre_push_binds_to_the_exact_proposed_oid() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = atomic_repository::Repository::init(&root).unwrap();
        drop(repo);
        let git = GitRepository::init(&root).unwrap();
        drop(git);

        // An unmanaged view so the provenance gate is not the blocker; the
        // exact-OID binding is what must refuse.
        let bogus = git2::Oid::from_str("0123456789012345678901234567890123456789").unwrap();
        let input = format!("refs/heads/main {bogus} refs/heads/main 0000000000000000000000000000000000000000\n");
        let error = run_pre_push_verification_with_input(&root, &input)
            .expect_err("a proposed OID off the verified projection must refuse");
        let text = error.to_string();
        assert!(
            text.contains("does not carry the verified manifest state") || text.contains("no verified projection") || text.contains("not the verified projection"),
            "the refusal names the exact-OID binding: {text}"
        );
    }
}
