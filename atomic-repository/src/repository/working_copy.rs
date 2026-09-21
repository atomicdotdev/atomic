//! Persistent working-copy identity and repository layout discovery.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use atomic_core::pristine::{
    MutTxnT, Pristine, PristineError, ViewTxnT, WorkingCopyMutTxnT, WorkingCopyRecord,
    WorkingCopyTxnT,
};
use atomic_core::types::Hash;
use atomic_core::WorkingCopyId;

use super::{Repository, DOT_DIR};
use crate::RepositoryError;

const REPOSITORY_POINTER: &str = "repository";
const WORKING_COPY_ID_FILE: &str = "working_copy_id";
const CURRENT_VIEW_FILE: &str = "current_view";
const LOCATION_FINGERPRINT_DOMAIN: &[u8] = b"atomic:working-copy-location:v1\0";

#[derive(Debug, Clone)]
pub(super) struct RepositoryLayout {
    pub working_root: PathBuf,
    pub common_dot_dir: PathBuf,
    pub working_copy_dot_dir: PathBuf,
    pub location_fingerprint: Hash,
    pub pointer_needs_write: bool,
}

#[derive(Debug, Clone)]
struct GitAdminPaths {
    worktree_root: PathBuf,
    common_dir: PathBuf,
    worktree_git_dir: PathBuf,
    index_path: PathBuf,
}

#[derive(Debug)]
enum IdentityFile {
    Missing,
    Empty,
    Valid(WorkingCopyId),
    Malformed(String),
}

/// CB-13D ::24 R1/R5: the public root detection for LINKED worktrees —
/// a tree whose `.git` pointer resolves to a common repository with a
/// `.atomic/` is a valid repository root even though it has no local
/// `.atomic/`. Returns the working root the CLI should treat as the
/// repository root, or `None` when `start` is not inside a repository.
/// CB-13D ::24 R5: the canonical common `.atomic` directory for `start`,
/// when `start` resolves inside a repository layout (linked worktrees
/// resolve to the common store; a plain repository to its own `.atomic`).
pub fn canonical_dot_dir_for(start: &Path) -> Option<PathBuf> {
    match discover_layout(start) {
        Ok(layout) => Some(layout.common_dot_dir),
        Err(_) => None,
    }
}

pub fn detect_repository_root(start: &Path) -> Result<Option<PathBuf>, RepositoryError> {
    match discover_layout(start) {
        Ok(layout) => Ok(Some(layout.working_root)),
        Err(RepositoryError::NotInRepository) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn discover_layout(start: &Path) -> Result<RepositoryLayout, RepositoryError> {
    let start = search_start(start)?;
    let home_dir = dirs::home_dir().and_then(|path| std::fs::canonicalize(path).ok());
    let mut current = Some(start.clone());

    while let Some(dir) = current {
        let local_dot_dir = dir.join(DOT_DIR);
        let repository_pointer = local_dot_dir.join(REPOSITORY_POINTER);
        if repository_pointer.is_file() {
            let common_dot_dir = read_repository_pointer(&repository_pointer)?;
            return layout_for_paths(dir, common_dot_dir, local_dot_dir, false);
        }

        if local_dot_dir.join("pristine.redb").is_file() {
            return layout_for_paths(dir, local_dot_dir.clone(), local_dot_dir, false);
        }

        if home_dir.as_deref() == Some(dir.as_path()) {
            break;
        }
        current = dir.parent().map(Path::to_path_buf);
    }

    if let Some(git) = resolve_git_admin(&start)? {
        let common_parent =
            git.common_dir
                .parent()
                .ok_or_else(|| RepositoryError::InvalidRepository {
                    reason: format!(
                        "resolved Git common directory '{}' has no parent",
                        git.common_dir.display()
                    ),
                })?;
        let common_dot_dir = common_parent.join(DOT_DIR);
        // CB-13D ::24 R1: linked-layout detection keys on the resolved Git
        // common directory's .atomic presence, not on `pristine.redb` file
        // existence — a linked worktree whose common store has not yet
        // created the database file (or is mid-migration) must still
        // resolve to its common layout instead of being misdetected.
        if common_dot_dir.is_dir() {
            let working_copy_dot_dir = git.worktree_root.join(DOT_DIR);
            return Ok(RepositoryLayout {
                working_root: git.worktree_root.clone(),
                common_dot_dir: canonical_existing(&common_dot_dir, "Atomic common directory")?,
                working_copy_dot_dir,
                location_fingerprint: fingerprint(&git.worktree_root, Some(&git)),
                pointer_needs_write: true,
            });
        }
    }

    Err(RepositoryError::NotFound {
        path: start.display().to_string(),
    })
}

pub(super) fn ensure_repository_pointer(layout: &RepositoryLayout) -> Result<(), RepositoryError> {
    if !layout.pointer_needs_write {
        return Ok(());
    }
    std::fs::create_dir_all(&layout.working_copy_dot_dir)?;
    atomic_write_text(
        &layout.working_copy_dot_dir,
        REPOSITORY_POINTER,
        &layout.common_dot_dir.display().to_string(),
    )
}

pub(super) fn registered_view_name(
    pristine: &Pristine,
    layout: &RepositoryLayout,
) -> Result<Option<String>, RepositoryError> {
    let parsed = read_identity_file(&layout.working_copy_dot_dir)?;
    if let IdentityFile::Malformed(reason) = parsed {
        return Err(malformed_identity_error(layout, reason));
    }

    let txn = pristine
        .read_txn()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let by_location = txn
        .find_working_copy_by_location(&layout.location_fingerprint)
        .map_err(|error| map_pristine_identity_error(layout, error))?;
    if let Some(record) = by_location {
        return resolve_record_view_name(&txn, &record).map(Some);
    }

    if let IdentityFile::Valid(id) = parsed {
        if let Some(record) = txn
            .get_working_copy(id)
            .map_err(|error| map_pristine_identity_error(layout, error))?
        {
            if record.location_fingerprint == layout.location_fingerprint {
                return resolve_record_view_name(&txn, &record).map(Some);
            }
        }
    }

    Ok(None)
}

pub(super) fn migrate_identity(
    pristine: &Pristine,
    layout: &RepositoryLayout,
    initial_view: &str,
) -> Result<(WorkingCopyId, String), RepositoryError> {
    let parsed = read_identity_file(&layout.working_copy_dot_dir)?;
    if let IdentityFile::Malformed(reason) = parsed {
        return Err(malformed_identity_error(layout, reason));
    }

    let mut txn = pristine
        .write_txn()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;

    let record = if let Some(record) = txn
        .find_working_copy_by_location(&layout.location_fingerprint)
        .map_err(|error| map_pristine_identity_error(layout, error))?
    {
        record
    } else {
        let matching_candidate = match parsed {
            IdentityFile::Valid(id) => txn
                .get_working_copy(id)
                .map_err(|error| map_pristine_identity_error(layout, error))?
                .filter(|record| record.location_fingerprint == layout.location_fingerprint),
            IdentityFile::Missing | IdentityFile::Empty | IdentityFile::Malformed(_) => None,
        };

        if let Some(record) = matching_candidate {
            record
        } else {
            let view = txn
                .get_view(initial_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: initial_view.to_string(),
                })?;
            let id = loop {
                let candidate = WorkingCopyId::new();
                if txn
                    .get_working_copy(candidate)
                    .map_err(|error| map_pristine_identity_error(layout, error))?
                    .is_none()
                {
                    break candidate;
                }
            };
            let record = WorkingCopyRecord {
                id,
                location_fingerprint: layout.location_fingerprint,
                desired_view: view.id,
                desired_state: view.state,
                materialized_state: None,
                materialized_manifest: None,
            };
            txn.put_working_copy(&record)
                .map_err(|error| map_pristine_identity_error(layout, error))?;
            record
        }
    };

    let current_view = resolve_record_view_name(&txn, &record)?;
    txn.commit()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;

    std::fs::create_dir_all(&layout.working_copy_dot_dir)?;
    atomic_write_text(
        &layout.working_copy_dot_dir,
        WORKING_COPY_ID_FILE,
        &record.id.to_string(),
    )?;
    write_current_view_compatibility(&layout.working_copy_dot_dir, &current_view)?;

    Ok((record.id, current_view))
}

pub(super) fn load_registered_identity(
    pristine: &Pristine,
    layout: &RepositoryLayout,
) -> Result<(WorkingCopyId, String), RepositoryError> {
    if layout.pointer_needs_write {
        return Err(migration_required(
            layout,
            "this linked Git worktree has not been registered with its common Atomic repository",
        ));
    }

    let id = match read_identity_file(&layout.working_copy_dot_dir)? {
        IdentityFile::Missing => {
            return Err(migration_required(layout, "working_copy_id is missing"))
        }
        IdentityFile::Empty => return Err(migration_required(layout, "working_copy_id is empty")),
        IdentityFile::Malformed(reason) => return Err(malformed_identity_error(layout, reason)),
        IdentityFile::Valid(id) => id,
    };

    let txn = pristine
        .read_txn()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let record = txn
        .get_working_copy(id)
        .map_err(|error| map_pristine_identity_error(layout, error))?
        .ok_or_else(|| {
            migration_required(
                layout,
                format!("working-copy record {id} is missing from pristine storage"),
            )
        })?;
    if record.location_fingerprint != layout.location_fingerprint {
        return Err(migration_required(
            layout,
            format!(
                "working-copy ID {id} belongs to a different canonical location (the directory may have been copied)"
            ),
        ));
    }
    let current_view = resolve_record_view_name(&txn, &record)?;
    Ok((id, current_view))
}

pub(super) fn write_current_view_compatibility(
    working_copy_dot_dir: &Path,
    view: &str,
) -> Result<(), RepositoryError> {
    atomic_write_text(working_copy_dot_dir, CURRENT_VIEW_FILE, view)
}

pub(super) fn layout_for_paths(
    working_root: PathBuf,
    common_dot_dir: PathBuf,
    working_copy_dot_dir: PathBuf,
    pointer_needs_write: bool,
) -> Result<RepositoryLayout, RepositoryError> {
    let working_root = canonical_existing(&working_root, "working-copy root")?;
    let common_dot_dir = canonical_existing(&common_dot_dir, "Atomic common directory")?;
    let git = resolve_git_admin(&working_root)?;
    Ok(RepositoryLayout {
        location_fingerprint: fingerprint(&working_root, git.as_ref()),
        working_root,
        common_dot_dir,
        working_copy_dot_dir,
        pointer_needs_write,
    })
}

fn search_start(start: &Path) -> Result<PathBuf, RepositoryError> {
    let start = if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    };
    if start.is_absolute() {
        Ok(std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf()))
    } else {
        let absolute = std::env::current_dir()?.join(start);
        Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
    }
}

fn read_repository_pointer(path: &Path) -> Result<PathBuf, RepositoryError> {
    let content =
        std::fs::read_to_string(path).map_err(|error| RepositoryError::InvalidRepository {
            reason: format!(
                "cannot read Atomic repository pointer '{}': {error}",
                path.display()
            ),
        })?;
    let value = content.trim();
    if value.is_empty() {
        return Err(RepositoryError::InvalidRepository {
            reason: format!("Atomic repository pointer '{}' is empty", path.display()),
        });
    }
    let pointed = PathBuf::from(value);
    let pointed = if pointed.is_absolute() {
        pointed
    } else {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(pointed)
    };
    let common_dot_dir = if pointed.join("pristine.redb").is_file() {
        pointed
    } else if pointed.join(DOT_DIR).join("pristine.redb").is_file() {
        pointed.join(DOT_DIR)
    } else {
        return Err(RepositoryError::InvalidRepository {
            reason: format!(
                "Atomic repository pointer '{}' does not reference a pristine database",
                path.display()
            ),
        });
    };
    canonical_existing(&common_dot_dir, "Atomic repository pointer target")
}

fn resolve_git_admin(root: &Path) -> Result<Option<GitAdminPaths>, RepositoryError> {
    let Some(worktree_output) = run_git_optional(root, &["rev-parse", "--show-toplevel"])? else {
        return Ok(None);
    };
    let worktree_root = resolve_git_path(root, &worktree_output, true)?;
    let common_dir = resolve_git_path(
        root,
        &run_git_required(root, &["rev-parse", "--git-common-dir"])?,
        true,
    )?;
    let worktree_git_dir = resolve_git_path(
        root,
        &run_git_required(root, &["rev-parse", "--git-dir"])?,
        true,
    )?;
    let index_path = resolve_git_path(
        root,
        &run_git_required(root, &["rev-parse", "--git-path", "index"])?,
        false,
    )?;

    Ok(Some(GitAdminPaths {
        worktree_root,
        common_dir,
        worktree_git_dir,
        index_path,
    }))
}

fn run_git_optional(root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>, RepositoryError> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_INDEX_FILE")
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(RepositoryError::Io(error)),
    };
    if output.status.success() {
        Ok(Some(trim_git_output(output.stdout)?))
    } else {
        Ok(None)
    }
}

fn run_git_required(root: &Path, args: &[&str]) -> Result<Vec<u8>, RepositoryError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        // CB-11A: the repository location fingerprint must not depend on an
        // alternate index selection. `git rev-parse --git-path index` honors
        // GIT_INDEX_FILE, so strip it here: the canonical primary index path
        // is an identity input, the selected index is only evidence.
        .env_remove("GIT_INDEX_FILE")
        .output()?;
    if !output.status.success() {
        return Err(RepositoryError::InvalidRepository {
            reason: format!(
                "git -C '{}' {} failed: {}",
                root.display(),
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    trim_git_output(output.stdout)
}

fn trim_git_output(mut bytes: Vec<u8>) -> Result<Vec<u8>, RepositoryError> {
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return Err(RepositoryError::InvalidRepository {
            reason: "Git returned a malformed administrative path".to_string(),
        });
    }
    Ok(bytes)
}

fn resolve_git_path(
    root: &Path,
    bytes: &[u8],
    must_exist: bool,
) -> Result<PathBuf, RepositoryError> {
    let raw = path_from_bytes(bytes)?;
    let path = if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    };
    if must_exist {
        canonical_existing(&path, "Git administrative path")
    } else {
        canonicalize_missing_leaf(&path)
    }
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, RepositoryError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, RepositoryError> {
    let value =
        String::from_utf8(bytes.to_vec()).map_err(|_| RepositoryError::InvalidRepository {
            reason: "Git returned a non-UTF-8 administrative path".to_string(),
        })?;
    Ok(PathBuf::from(value))
}

fn canonical_existing(path: &Path, label: &str) -> Result<PathBuf, RepositoryError> {
    std::fs::canonicalize(path).map_err(|error| RepositoryError::InvalidRepository {
        reason: format!("cannot resolve {label} '{}': {error}", path.display()),
    })
}

fn canonicalize_missing_leaf(path: &Path) -> Result<PathBuf, RepositoryError> {
    if path.exists() {
        return canonical_existing(path, "Git administrative path");
    }
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidRepository {
            reason: format!("Git administrative path '{}' has no parent", path.display()),
        })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| RepositoryError::InvalidRepository {
            reason: format!(
                "Git administrative path '{}' has no file name",
                path.display()
            ),
        })?;
    Ok(canonical_existing(parent, "Git administrative directory")?.join(file_name))
}

fn fingerprint(root: &Path, git: Option<&GitAdminPaths>) -> Hash {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(LOCATION_FINGERPRINT_DOMAIN);
    push_path(&mut bytes, root);
    match git {
        Some(git) => {
            bytes.push(1);
            push_path(&mut bytes, &git.common_dir);
            push_path(&mut bytes, &git.worktree_git_dir);
            push_path(&mut bytes, &git.index_path);
        }
        None => {
            // A native repository may later enable the ordinary colocated Git
            // backend. Bind its initial identity to the administrative paths a
            // normal `git init` at this root will create so that adding Git does
            // not look like a copied working directory. Linked worktrees still
            // hash their distinct resolved per-worktree Git directory and index.
            bytes.push(1);
            let git_dir = root.join(".git");
            push_path(&mut bytes, &git_dir);
            push_path(&mut bytes, &git_dir);
            push_path(&mut bytes, &git_dir.join("index"));
        }
    }
    Hash::of(&bytes)
}

fn push_path(bytes: &mut Vec<u8>, path: &Path) {
    let encoded = os_path_bytes(path.as_os_str());
    bytes.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&encoded);
}

#[cfg(unix)]
fn os_path_bytes(path: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_path_bytes(path: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}

#[cfg(not(any(unix, windows)))]
fn os_path_bytes(path: &std::ffi::OsStr) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

fn read_identity_file(working_copy_dot_dir: &Path) -> Result<IdentityFile, RepositoryError> {
    let path = working_copy_dot_dir.join(WORKING_COPY_ID_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(IdentityFile::Missing)
        }
        Err(error) => return Err(RepositoryError::Io(error)),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Ok(IdentityFile::Malformed(
            "identity path is not a regular file".to_string(),
        ));
    }
    let bytes = std::fs::read(&path)?;
    let value = match String::from_utf8(bytes) {
        Ok(value) => value,
        Err(_) => {
            return Ok(IdentityFile::Malformed(
                "identity is not valid UTF-8".to_string(),
            ))
        }
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(IdentityFile::Empty);
    }
    match value.parse::<WorkingCopyId>() {
        Ok(id) => Ok(IdentityFile::Valid(id)),
        Err(error) => Ok(IdentityFile::Malformed(format!(
            "'{value}' is not a canonical ULID: {error}"
        ))),
    }
}

fn resolve_record_view_name<T: ViewTxnT>(
    txn: &T,
    record: &WorkingCopyRecord,
) -> Result<String, RepositoryError> {
    txn.get_view_by_id(record.desired_view)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
        .map(|view| view.name)
        .ok_or_else(|| RepositoryError::InvalidRepository {
            reason: format!(
                "working-copy record {} references missing desired view {}",
                record.id, record.desired_view
            ),
        })
}

fn atomic_write_text(dir: &Path, name: &str, value: &str) -> Result<(), RepositoryError> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.as_file_mut().write_all(value.as_bytes())?;
    temp.as_file_mut().write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(&path).map_err(|error| {
        RepositoryError::Io(std::io::Error::other(format!(
            "failed to persist '{}': {}",
            path.display(),
            error
        )))
    })?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn migration_required(layout: &RepositoryLayout, reason: impl Into<String>) -> RepositoryError {
    RepositoryError::WorkingCopyMigrationRequired {
        path: layout.working_copy_dot_dir.join(WORKING_COPY_ID_FILE),
        reason: reason.into(),
    }
}

fn malformed_identity_error(
    layout: &RepositoryLayout,
    reason: impl Into<String>,
) -> RepositoryError {
    RepositoryError::MalformedWorkingCopyIdentity {
        path: layout.working_copy_dot_dir.join(WORKING_COPY_ID_FILE),
        reason: reason.into(),
    }
}

fn map_pristine_identity_error(layout: &RepositoryLayout, error: PristineError) -> RepositoryError {
    match error {
        PristineError::WorkingCopySchemaUnavailable => {
            migration_required(layout, "the pristine WORKING_COPIES schema is unavailable")
        }
        other => RepositoryError::Database(other.to_string()),
    }
}

impl Repository {
    /// Per-directory metadata path for this physical working copy.
    pub fn working_copy_dot_dir(&self) -> PathBuf {
        self.root.join(DOT_DIR)
    }

    /// Read the stable identity associated with this working directory.
    pub fn working_copy_id(&self) -> Option<WorkingCopyId> {
        match read_identity_file(&self.working_copy_dot_dir()).ok()? {
            IdentityFile::Valid(id) => Some(id),
            IdentityFile::Missing | IdentityFile::Empty | IdentityFile::Malformed(_) => None,
        }
    }

    /// Return this working directory's identity or an actionable typed error.
    pub fn require_working_copy_id(&self) -> Result<WorkingCopyId, RepositoryError> {
        let layout = layout_for_paths(
            self.root.clone(),
            self.dot_dir.clone(),
            self.working_copy_dot_dir(),
            false,
        )?;
        match read_identity_file(&layout.working_copy_dot_dir)? {
            IdentityFile::Valid(id) => Ok(id),
            IdentityFile::Missing => Err(migration_required(&layout, "working_copy_id is missing")),
            IdentityFile::Empty => Err(migration_required(&layout, "working_copy_id is empty")),
            IdentityFile::Malformed(reason) => Err(malformed_identity_error(&layout, reason)),
        }
    }

    /// Load one persistent working-copy record.
    pub fn working_copy_record(
        &self,
        id: WorkingCopyId,
    ) -> Result<WorkingCopyRecord, RepositoryError> {
        let layout = layout_for_paths(
            self.root.clone(),
            self.dot_dir.clone(),
            self.working_copy_dot_dir(),
            false,
        )?;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        txn.get_working_copy(id)
            .map_err(|error| map_pristine_identity_error(&layout, error))?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id })
    }

    /// Verify that an ID, its record, and this canonical location all agree.
    pub fn validate_working_copy(&self, id: WorkingCopyId) -> Result<(), RepositoryError> {
        let actual = self.require_working_copy_id()?;
        if actual != id {
            return Err(RepositoryError::WorkingCopyIdentityMismatch {
                requested: id,
                actual,
            });
        }
        let layout = layout_for_paths(
            self.root.clone(),
            self.dot_dir.clone(),
            self.working_copy_dot_dir(),
            false,
        )?;
        let record = self.working_copy_record(id)?;
        if record.location_fingerprint != layout.location_fingerprint {
            return Err(RepositoryError::WorkingCopyLocationMismatch { id });
        }
        Ok(())
    }

    /// Resolve the authoritative desired view for a validated working copy.
    pub fn desired_view_name(&self, id: WorkingCopyId) -> Result<String, RepositoryError> {
        self.validate_working_copy(id)?;
        let record = self.working_copy_record(id)?;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        resolve_record_view_name(&txn, &record)
    }

    pub(super) fn update_working_copy_desired_view(
        &self,
        id: WorkingCopyId,
        view_name: &str,
    ) -> Result<(), RepositoryError> {
        self.validate_working_copy(id)?;
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let mut record = txn
            .get_working_copy(id)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id })?;
        record.desired_view = view.id;
        record.desired_state = view.state;
        record.materialized_state = None;
        record.materialized_manifest = None;
        txn.put_working_copy(&record)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(())
    }

    pub(super) fn refresh_working_copy_desired_state(
        &self,
        id: WorkingCopyId,
    ) -> Result<(), RepositoryError> {
        self.validate_working_copy(id)?;
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let mut record = txn
            .get_working_copy(id)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id })?;
        let view = ViewTxnT::get_view_by_id(&txn, record.desired_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::InvalidRepository {
                reason: format!(
                    "working-copy record {id} references missing desired view {}",
                    record.desired_view
                ),
            })?;
        record.desired_state = view.state;
        txn.put_working_copy(&record)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(())
    }
}
