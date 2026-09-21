//! Read-only Git index and physical worktree observation for CB-4B.

use super::project_tree::{git_object_id, GitObjectKind};
use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_core::Hash;
use atomic_objects::content_key;
use thiserror::Error;
use walkdir::WalkDir;

use crate::content_filter::ContentFilter;

const ASSUME_VALID: u16 = 0x8000;
const INTENT_TO_ADD: u16 = 0x2000;
const SKIP_WORKTREE: u16 = 0x4000;

/// Fail-closed errors produced before an observation can be trusted.
#[derive(Debug, Error)]
pub enum ObservationError {
    #[error("cannot open Git repository at '{path}': {message}")]
    GitOpen { path: String, message: String },
    #[error("cannot read Git index: {0}")]
    GitIndex(String),
    #[error("Git object format '{observed}' does not match policy {expected:?}")]
    ObjectFormat {
        observed: String,
        expected: GitHashAlgorithm,
    },
    #[error("Git index object ID width is unsupported for {0:?}")]
    UnsupportedObjectFormat(GitHashAlgorithm),
    #[error("invalid raw repository path: {0}")]
    InvalidPath(String),
    #[error("cannot inspect worktree path '{path}': {message}")]
    WorktreeIo { path: String, message: String },
    #[error("content clean failed for '{path}': {message}")]
    Filter { path: String, message: String },
    #[error("platform cannot represent lossless Unix repository paths")]
    UnsupportedPlatform,
    #[error("observed platform capabilities differ from conversion policy: expected {expected}, observed {observed}")]
    PlatformMismatch { expected: String, observed: String },
    #[error("cannot compute index tree: {0}")]
    IndexTree(String),
}

/// Read a Git index without refreshing, locking, or writing it.
pub fn observe_git_index(
    root: &Path,
    policy: &ConversionPolicy,
) -> Result<GitIndexState, ObservationError> {
    let repository =
        git2::Repository::discover(root).map_err(|error| ObservationError::GitOpen {
            path: root.display().to_string(),
            message: error.to_string(),
        })?;
    let observed_algorithm = repository_object_algorithm(&repository)?;
    if observed_algorithm != policy.object_format {
        return Err(ObservationError::ObjectFormat {
            observed: format!("{observed_algorithm:?}"),
            expected: policy.object_format,
        });
    }
    match repository.index() {
        Ok(index) => observe_open_git_index(observed_algorithm, &index),
        // libgit2 cannot parse indexes carrying mandatory extensions it does
        // not implement (e.g. `sdir` sparse-directory entries in sparse
        // indexes). Fall back to read-only plumbing so observation keeps
        // working instead of failing status in sparse-checkout repositories.
        Err(_) => observe_git_index_via_plumbing(root, None, observed_algorithm),
    }
}

/// Observe an explicitly selected index file (e.g. an alternate
/// `GIT_INDEX_FILE`) without refreshing, locking, or writing it.
pub fn observe_git_index_path(
    root: &Path,
    index_path: &Path,
    policy: &ConversionPolicy,
) -> Result<GitIndexState, ObservationError> {
    let repository =
        git2::Repository::discover(root).map_err(|error| ObservationError::GitOpen {
            path: root.display().to_string(),
            message: error.to_string(),
        })?;
    let observed_algorithm = repository_object_algorithm(&repository)?;
    if observed_algorithm != policy.object_format {
        return Err(ObservationError::ObjectFormat {
            observed: format!("{observed_algorithm:?}"),
            expected: policy.object_format,
        });
    }
    match git2::Index::open(index_path) {
        Ok(index) => observe_open_git_index(observed_algorithm, &index),
        Err(_) => observe_git_index_via_plumbing(root, Some(index_path), observed_algorithm),
    }
}

/// Read-only index observation through `git ls-files` plumbing, for index
/// formats libgit2 cannot parse. Nothing is written: `--no-optional-locks`
/// prevents refresh-time index writes, and `ls-files`/`status` only read.
fn observe_git_index_via_plumbing(
    root: &Path,
    index_path: Option<&Path>,
    algorithm: GitHashAlgorithm,
) -> Result<GitIndexState, ObservationError> {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--stage", "-t", "-z"]);
    match index_path {
        Some(path) => {
            command.env("GIT_INDEX_FILE", path);
        }
        None => {
            command.env_remove("GIT_INDEX_FILE");
        }
    }
    let output = command
        .output()
        .map_err(|error| ObservationError::GitIndex(format!("cannot run git ls-files: {error}")))?;
    if !output.status.success() {
        return Err(ObservationError::GitIndex(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let mut entries = Vec::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(record);
        // `<tag> <mode> <oid> <stage>\t<path>`
        let Some((meta, path)) = line.split_once('\t') else {
            return Err(ObservationError::GitIndex(format!(
                "cannot parse ls-files record: {line:?}"
            )));
        };
        let mut parts = meta.splitn(4, ' ');
        let tag = parts.next().unwrap_or("H");
        let Some(mode) = parts
            .next()
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
        else {
            return Err(ObservationError::GitIndex(format!(
                "cannot parse ls-files mode: {line:?}"
            )));
        };
        let oid_hex = parts.next().unwrap_or_default();
        let stage = parts
            .next()
            .and_then(|stage| stage.parse::<u8>().ok())
            .unwrap_or(0);
        let oid = parse_git_oid_hex(algorithm, oid_hex)?;
        let path = RepoPath::from_bytes(path.as_bytes())
            .map_err(|error| ObservationError::InvalidPath(error.to_string()))?;
        entries.push(GitIndexEntry {
            path,
            stage,
            mode: canonical_index_mode(mode),
            oid: Some(oid),
            intent_to_add: false,
            skip_worktree: tag == "S",
            // git ls-files -t reports assume-unchanged entries in lowercase.
            assume_unchanged: tag
                .chars()
                .next()
                .is_some_and(|tag| tag.is_ascii_lowercase()),
            sparse_directory: mode & 0o170000 == 0o040000,
        });
    }

    // Intent-to-add records intent only; porcelain surfaces it as an
    // unstaged add (`XY` = ` A`). Read-only via --no-optional-locks.
    let mut status_command = Command::new("git");
    status_command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root);
    match index_path {
        Some(path) => {
            status_command.env("GIT_INDEX_FILE", path);
        }
        None => {
            status_command.env_remove("GIT_INDEX_FILE");
        }
    }
    status_command.args(["status", "--porcelain=v1", "-z", "--untracked-files=no"]);
    if let Ok(status_output) = status_command.output() {
        if status_output.status.success() {
            for record in status_output.stdout.split(|byte| *byte == 0) {
                if record.len() < 3 {
                    continue;
                }
                let (columns, path) = record.split_at(2);
                if columns == b" A" {
                    for entry in &mut entries {
                        if entry.path.as_bytes() == path {
                            entry.intent_to_add = true;
                        }
                    }
                }
            }
        }
    }

    entries.sort_by(|left, right| {
        (&left.path, left.stage, &left.oid).cmp(&(&right.path, right.stage, &right.oid))
    });
    let tree = compute_index_tree(algorithm, &entries)?;
    // Detect the sparse-directory extension in the observed index file.
    let index_file = index_path.map_or_else(|| repository_path_index(root), Path::to_path_buf);
    let sparse_index = fs::read(&index_file)
        .map(|bytes| bytes.windows(4).any(|window| window == b"sdir"))
        .unwrap_or(false);
    Ok(GitIndexState {
        version: GIT_INDEX_STATE_VERSION,
        index_version: 0,
        object_format: algorithm,
        entries,
        tree,
        sparse_index,
    })
}

fn repository_path_index(root: &Path) -> PathBuf {
    git2::Repository::discover(root)
        .map(|repository| repository.path().join("index"))
        .unwrap_or_else(|_| root.join(".git").join("index"))
}

/// Parse a lowercase hex Git object ID into an algorithm-tagged identity.
fn parse_git_oid_hex(
    algorithm: GitHashAlgorithm,
    hex: &str,
) -> Result<GitObjectId, ObservationError> {
    if !hex.len().is_multiple_of(2) {
        return Err(ObservationError::GitIndex(format!(
            "odd-length object id: {hex}"
        )));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let byte_pairs = hex.as_bytes();
    for pair in byte_pairs.chunks(2) {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| ObservationError::GitIndex(format!("invalid hex digit in {hex}")))?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| ObservationError::GitIndex(format!("invalid hex digit in {hex}")))?;
        bytes.push(((high << 4) | low) as u8);
    }
    GitObjectId::new(algorithm, bytes)
        .map_err(|error| ObservationError::GitIndex(error.to_string()))
}

pub(super) fn observe_open_git_index(
    observed_algorithm: GitHashAlgorithm,
    index: &git2::Index,
) -> Result<GitIndexState, ObservationError> {
    let mut entries = Vec::with_capacity(index.len());
    for entry in index.iter() {
        let path = RepoPath::from_bytes(&entry.path)
            .map_err(|error| ObservationError::InvalidPath(error.to_string()))?;
        let oid = GitObjectId::new(observed_algorithm, entry.id.as_bytes().to_vec())
            .map_err(|_| ObservationError::UnsupportedObjectFormat(observed_algorithm))?;
        entries.push(GitIndexEntry {
            path,
            stage: ((entry.flags >> 12) & 0x3) as u8,
            mode: canonical_index_mode(entry.mode),
            oid: Some(oid),
            intent_to_add: entry.flags_extended & INTENT_TO_ADD != 0,
            skip_worktree: entry.flags_extended & SKIP_WORKTREE != 0,
            assume_unchanged: entry.flags & ASSUME_VALID != 0,
            sparse_directory: entry.mode & 0o170000 == 0o040000,
        });
    }
    entries.sort_by(|left, right| {
        (&left.path, left.stage, &left.oid).cmp(&(&right.path, right.stage, &right.oid))
    });
    let tree = compute_index_tree(observed_algorithm, &entries)?;
    Ok(GitIndexState {
        version: GIT_INDEX_STATE_VERSION,
        index_version: index.version(),
        object_format: observed_algorithm,
        entries,
        tree,
        sparse_index: false,
    })
}

fn repository_object_algorithm(
    repository: &git2::Repository,
) -> Result<GitHashAlgorithm, ObservationError> {
    let config = repository
        .config()
        .map_err(|error| ObservationError::GitIndex(error.to_string()))?;
    let value = match config.get_string("extensions.objectFormat") {
        Ok(value) => value,
        Err(error) if error.code() == git2::ErrorCode::NotFound => "sha1".to_string(),
        Err(error) => return Err(ObservationError::GitIndex(error.to_string())),
    };
    match value.to_ascii_lowercase().as_str() {
        "sha1" => Ok(GitHashAlgorithm::Sha1),
        "sha256" => Ok(GitHashAlgorithm::Sha256),
        _ => Err(ObservationError::ObjectFormat {
            observed: value,
            expected: GitHashAlgorithm::Sha1,
        }),
    }
}

fn canonical_index_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o040000 => 0o040000,
        0o100000 if mode & 0o111 == 0 => 0o100644,
        0o100000 => 0o100755,
        0o120000 => 0o120000,
        0o160000 => 0o160000,
        _ => mode,
    }
}

#[derive(Default)]
struct IndexDirectory {
    entries: BTreeMap<Vec<u8>, IndexNode>,
}

enum IndexNode {
    Directory(IndexDirectory),
    Object { mode: u32, oid: GitObjectId },
}

pub(super) fn compute_index_tree(
    algorithm: GitHashAlgorithm,
    entries: &[GitIndexEntry],
) -> Result<Option<GitObjectId>, ObservationError> {
    if entries.iter().any(|entry| {
        entry.stage != 0
            || entry.intent_to_add
            || entry.oid.is_none()
            || entry
                .oid
                .as_ref()
                .is_some_and(|oid| oid.as_bytes().iter().all(|byte| *byte == 0))
    }) {
        return Ok(None);
    }
    let mut root = IndexDirectory::default();
    for entry in entries {
        let oid = entry.oid.clone().expect("checked above");
        if oid.algorithm() != algorithm {
            return Err(ObservationError::UnsupportedObjectFormat(algorithm));
        }
        insert_index_entry(
            &mut root,
            entry.path.components().collect(),
            entry.mode,
            oid,
        )?;
    }
    hash_index_directory(algorithm, &root).map(Some)
}

fn insert_index_entry(
    root: &mut IndexDirectory,
    components: Vec<&[u8]>,
    mode: u32,
    oid: GitObjectId,
) -> Result<(), ObservationError> {
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| ObservationError::IndexTree("empty path".into()))?;
    let mut directory = root;
    for parent in parents {
        let node = directory
            .entries
            .entry(parent.to_vec())
            .or_insert_with(|| IndexNode::Directory(IndexDirectory::default()));
        match node {
            IndexNode::Directory(child) => directory = child,
            IndexNode::Object { .. } => {
                return Err(ObservationError::IndexTree(
                    "file/directory path collision".into(),
                ))
            }
        }
    }
    let node = if mode == 0o040000 || matches!(mode, 0o100644 | 0o100755 | 0o120000 | 0o160000) {
        IndexNode::Object { mode, oid }
    } else {
        return Err(ObservationError::IndexTree(format!(
            "unsupported mode {mode:#o}"
        )));
    };
    if directory.entries.insert(name.to_vec(), node).is_some() {
        return Err(ObservationError::IndexTree("duplicate path".into()));
    }
    Ok(())
}

fn hash_index_directory(
    algorithm: GitHashAlgorithm,
    directory: &IndexDirectory,
) -> Result<GitObjectId, ObservationError> {
    struct Encoded {
        name: Vec<u8>,
        mode: u32,
        oid: GitObjectId,
    }
    let mut encoded = Vec::new();
    for (name, node) in &directory.entries {
        match node {
            IndexNode::Directory(child) => encoded.push(Encoded {
                name: name.clone(),
                mode: 0o040000,
                oid: hash_index_directory(algorithm, child)?,
            }),
            IndexNode::Object { mode, oid } => encoded.push(Encoded {
                name: name.clone(),
                mode: *mode,
                oid: oid.clone(),
            }),
        }
    }
    encoded.sort_by(|left, right| {
        git_name_order(
            &left.name,
            left.mode == 0o040000,
            &right.name,
            right.mode == 0o040000,
        )
    });
    let mut bytes = Vec::new();
    for entry in encoded {
        bytes.extend_from_slice(format!("{:o} ", entry.mode).as_bytes());
        bytes.extend_from_slice(&entry.name);
        bytes.push(0);
        bytes.extend_from_slice(entry.oid.as_bytes());
    }
    git_object_id(algorithm, GitObjectKind::Tree, &bytes)
        .map_err(|error| ObservationError::IndexTree(error.to_string()))
}

fn git_name_order(
    left: &[u8],
    left_tree: bool,
    right: &[u8],
    right_tree: bool,
) -> std::cmp::Ordering {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    if left_tree {
        left.push(b'/');
    }
    if right_tree {
        right.push(b'/');
    }
    left.cmp(&right)
}

/// Observe physical worktree state without following symlinks.
///
/// Gitlink directories are retained as single explicit entries when identified
/// by the index; their contents are never traversed.
pub fn observe_worktree(
    root: &Path,
    index: Option<&GitIndexState>,
    filter: &dyn ContentFilter,
    policy: &ConversionPolicy,
) -> Result<WorktreeObservation, ObservationError> {
    if !cfg!(unix) || !policy.platform.lossless_unix_paths {
        return Err(ObservationError::UnsupportedPlatform);
    }
    let platform = detect_platform_capabilities(root, policy)?;
    if platform != policy.platform {
        return Err(ObservationError::PlatformMismatch {
            expected: format!("{:?}", policy.platform),
            observed: format!("{platform:?}"),
        });
    }
    let stage_zero: BTreeMap<RepoPath, &GitIndexEntry> = index
        .into_iter()
        .flat_map(|state| state.entries.iter())
        .filter(|entry| entry.stage == 0)
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let gitlinks: BTreeSet<RepoPath> = stage_zero
        .iter()
        .filter(|(_, entry)| entry.mode == 0o160000)
        .map(|(path, _)| path.clone())
        .collect();
    let mut entries = Vec::new();
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(next) = walker.next() {
        let entry = next.map_err(|error| ObservationError::WorktreeIo {
            path: error.path().unwrap_or(root).display().to_string(),
            message: error.to_string(),
        })?;
        if entry.path() == root {
            continue;
        }
        let relative =
            entry
                .path()
                .strip_prefix(root)
                .map_err(|error| ObservationError::WorktreeIo {
                    path: entry.path().display().to_string(),
                    message: error.to_string(),
                })?;
        let path = RepoPath::from_native(relative)
            .map_err(|error| ObservationError::InvalidPath(error.to_string()))?;
        let first = path.components().next().unwrap_or_default();
        if first == b".git" || first == b".atomic" {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|error| ObservationError::WorktreeIo {
                path: path.escaped(),
                message: error.to_string(),
            })?;
        let is_gitlink = gitlinks.contains(&path);
        if metadata.is_dir() && !is_gitlink {
            continue;
        }
        if is_gitlink {
            walker.skip_current_dir();
        }
        let physical_kind = if metadata.file_type().is_symlink() {
            PhysicalKind::Symlink
        } else if metadata.is_file() {
            PhysicalKind::Regular
        } else if metadata.is_dir() {
            PhysicalKind::Directory
        } else {
            PhysicalKind::Other
        };
        let worktree_bytes = match physical_kind {
            PhysicalKind::Regular => {
                fs::read(entry.path()).map_err(|error| ObservationError::WorktreeIo {
                    path: path.escaped(),
                    message: error.to_string(),
                })?
            }
            PhysicalKind::Symlink => symlink_target_bytes(entry.path(), &path)?,
            PhysicalKind::Directory | PhysicalKind::Other => Vec::new(),
        };
        let (repository_bytes_after_clean, filter_warnings) = match physical_kind {
            PhysicalKind::Regular => {
                let filtered = filter.clean(relative, &worktree_bytes).map_err(|error| {
                    ObservationError::Filter {
                        path: path.escaped(),
                        message: error.to_string(),
                    }
                })?;
                (Some(filtered.bytes), filtered.warnings)
            }
            PhysicalKind::Symlink => (Some(worktree_bytes.clone()), Vec::new()),
            PhysicalKind::Directory | PhysicalKind::Other => (None, Vec::new()),
        };
        let repository_content_after_clean =
            repository_bytes_after_clean.as_deref().map(content_key);
        let disposition = policy
            .exclusions
            .exclusion(&path)
            .map(ManifestDisposition::Excluded)
            .unwrap_or(ManifestDisposition::Included);
        let indexed = stage_zero.get(&path).copied();
        let repository_kind = indexed.and_then(|entry| match entry.mode {
            0o100644 | 0o100755 => Some(atomic_core::change::InodeKind::Regular),
            0o120000 => Some(atomic_core::change::InodeKind::Symlink),
            0o160000 => Some(atomic_core::change::InodeKind::Gitlink),
            _ => None,
        });
        let gitlink = indexed
            .filter(|entry| entry.mode == 0o160000)
            .and_then(|entry| entry.oid.clone());
        entries.push(WorktreeEntry {
            path,
            physical_kind,
            repository_kind,
            gitlink,
            mode: unix_mode(&metadata),
            size: metadata.len(),
            worktree_content: content_key(&worktree_bytes),
            worktree_bytes,
            repository_bytes_after_clean,
            repository_content_after_clean,
            disposition,
            filter_warnings,
            filter_error: None,
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(WorktreeObservation::new(platform, entries))
}

#[cfg(unix)]
fn symlink_target_bytes(path: &Path, repo_path: &RepoPath) -> Result<Vec<u8>, ObservationError> {
    use std::os::unix::ffi::OsStrExt;
    fs::read_link(path)
        .map(|target| target.as_os_str().as_bytes().to_vec())
        .map_err(|error| ObservationError::WorktreeIo {
            path: repo_path.escaped(),
            message: error.to_string(),
        })
}

#[cfg(not(unix))]
fn symlink_target_bytes(_path: &Path, _repo_path: &RepoPath) -> Result<Vec<u8>, ObservationError> {
    Err(ObservationError::UnsupportedPlatform)
}

#[cfg(unix)]
fn unix_mode(metadata: &fs::Metadata) -> Option<u16> {
    use std::os::unix::fs::PermissionsExt;
    Some((metadata.permissions().mode() & 0o777) as u16)
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &fs::Metadata) -> Option<u16> {
    None
}

fn detect_platform_capabilities(
    root: &Path,
    policy: &ConversionPolicy,
) -> Result<PlatformCapabilities, ObservationError> {
    let mut capabilities = policy.platform.clone();
    capabilities.lossless_unix_paths = cfg!(unix);
    if let Ok(repository) = git2::Repository::discover(root) {
        let config = repository
            .config()
            .map_err(|error| ObservationError::GitIndex(error.to_string()))?;
        capabilities.executable_bit = config.get_bool("core.filemode").unwrap_or(cfg!(unix));
        capabilities.symlinks = config.get_bool("core.symlinks").unwrap_or(cfg!(unix));
        capabilities.case_sensitive = !config.get_bool("core.ignorecase").unwrap_or(false);
        capabilities.unicode_normalizing =
            config.get_bool("core.precomposeunicode").unwrap_or(false);
    }
    Ok(capabilities)
}

/// Complete read-only Git metadata needed at a workspace transaction boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceGitObservation {
    NoGit { root: PathBuf },
    Repository(Box<WorkspaceGitRepositoryObservation>),
}

impl WorkspaceGitObservation {
    pub fn token(&self) -> GitObservationToken {
        match self {
            Self::NoGit { .. } => GitObservationToken {
                head: GitHeadObservation::Unborn {
                    symref: "no-git".to_string(),
                },
                head_tree: None,
                index_digest: Hash::ZERO,
                index_tree: None,
                index_stages: Vec::new(),
                index_locked: false,
                repository_state: "NoGit".to_string(),
                markers: Vec::new(),
            },
            Self::Repository(repository) => GitObservationToken {
                head: repository.head.clone(),
                head_tree: repository.head_tree.clone(),
                index_digest: repository.index_digest,
                index_tree: repository.index_tree.clone(),
                index_stages: repository.index_stages.clone(),
                index_locked: repository.index_lock.is_present(),
                repository_state: repository.operation.repository_state.clone(),
                markers: repository.operation.present_markers(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceGitRepositoryObservation {
    pub worktree_git_dir: PathBuf,
    pub common_dir: PathBuf,
    pub index_path: PathBuf,
    pub head: GitHeadObservation,
    pub head_tree: Option<String>,
    pub index_digest: Hash,
    pub index_tree: Option<String>,
    pub index_stages: Vec<u8>,
    pub index_lock: GitAdminPathObservation,
    pub operation: GitOperationObservation,
}

impl WorkspaceGitRepositoryObservation {
    pub fn conflict_stages(&self) -> Vec<u8> {
        self.index_stages
            .iter()
            .copied()
            .filter(|stage| *stage != 0)
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitObservationToken {
    pub head: GitHeadObservation,
    pub head_tree: Option<String>,
    pub index_digest: Hash,
    pub index_tree: Option<String>,
    pub index_stages: Vec<u8>,
    pub index_locked: bool,
    pub repository_state: String,
    pub markers: Vec<GitOperationMarker>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitHeadObservation {
    Attached { symref: String, oid: String },
    Detached { oid: String },
    Unborn { symref: String },
    MissingTarget { symref: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitAdminEntryKind {
    Missing,
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitAdminPathObservation {
    pub path: PathBuf,
    pub kind: GitAdminEntryKind,
}

impl GitAdminPathObservation {
    pub fn is_present(&self) -> bool {
        self.kind != GitAdminEntryKind::Missing
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum GitOperationMarker {
    Sequencer,
    MergeHead,
    RebaseHead,
    RebaseMerge,
    RebaseApply,
    CherryPickHead,
    RevertHead,
    AutoMerge,
    BisectStart,
    BisectLog,
    BisectNames,
    BisectExpectedRev,
}

impl GitOperationMarker {
    pub(crate) const ALL: [Self; 12] = [
        Self::Sequencer,
        Self::MergeHead,
        Self::RebaseHead,
        Self::RebaseMerge,
        Self::RebaseApply,
        Self::CherryPickHead,
        Self::RevertHead,
        Self::AutoMerge,
        Self::BisectStart,
        Self::BisectLog,
        Self::BisectNames,
        Self::BisectExpectedRev,
    ];

    /// Whether a present marker is *authoritative* evidence that Git owns an
    /// in-progress operation.
    ///
    /// `AUTO_MERGE` is the one advisory marker: merge-ort writes it as a root
    /// ref (`refs_update_ref(..., "AUTO_MERGE", ...)`) for **any**
    /// worktree-updating merge — clean or conflicted — and `git merge` removes
    /// it with the rest of the merge state on completion or abort. It is a
    /// derived tree reference used to reconstruct the auto-merged result, not
    /// proof that a merge is still running. A merge that actually stopped
    /// before committing also has `MERGE_HEAD` (written by `write_merge_state`),
    /// so an AUTO_MERGE left alone is a stale diagnostic of a completed or
    /// interrupted cleanup. Every other marker names a live operation and stays
    /// authoritative; a non-`Clean` libgit2 repository state still refuses
    /// independently, as does any unmerged index stage.
    pub(crate) fn is_active_operation_evidence(self) -> bool {
        !matches!(self, Self::AutoMerge)
    }

    pub(crate) fn relative_path(self) -> &'static str {
        match self {
            Self::Sequencer => "sequencer",
            Self::MergeHead => "MERGE_HEAD",
            Self::RebaseHead => "REBASE_HEAD",
            Self::RebaseMerge => "rebase-merge",
            Self::RebaseApply => "rebase-apply",
            Self::CherryPickHead => "CHERRY_PICK_HEAD",
            Self::RevertHead => "REVERT_HEAD",
            Self::AutoMerge => "AUTO_MERGE",
            Self::BisectStart => "BISECT_START",
            Self::BisectLog => "BISECT_LOG",
            Self::BisectNames => "BISECT_NAMES",
            Self::BisectExpectedRev => "BISECT_EXPECTED_REV",
        }
    }
}

impl std::fmt::Display for GitOperationMarker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.relative_path())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitOperationMarkerObservation {
    pub marker: GitOperationMarker,
    pub path: PathBuf,
    pub kind: GitAdminEntryKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitOperationObservation {
    pub repository_state: String,
    pub markers: Vec<GitOperationMarkerObservation>,
}

impl GitOperationObservation {
    /// Whether Git owns an in-progress operation that must fence a workspace
    /// mutation.
    ///
    /// True when libgit2 reports a non-`Clean` repository state, or when any
    /// present marker is authoritative evidence of an active operation
    /// (`is_active_operation_evidence`). A standalone
    /// `AUTO_MERGE` does not, by itself, count: it is advisory.
    pub fn is_in_progress(&self) -> bool {
        self.repository_state != "Clean"
            || self.markers.iter().any(|marker| {
                marker.kind != GitAdminEntryKind::Missing
                    && marker.marker.is_active_operation_evidence()
            })
    }

    /// Every present marker, including advisory ones, for reporting.
    pub fn present_markers(&self) -> Vec<GitOperationMarker> {
        self.markers
            .iter()
            .filter(|marker| marker.kind != GitAdminEntryKind::Missing)
            .map(|marker| marker.marker)
            .collect()
    }

    /// Present markers that are authoritative evidence of an active operation.
    pub fn active_markers(&self) -> Vec<GitOperationMarker> {
        self.markers
            .iter()
            .filter(|marker| {
                marker.kind != GitAdminEntryKind::Missing
                    && marker.marker.is_active_operation_evidence()
            })
            .map(|marker| marker.marker)
            .collect()
    }
}

/// Canonical, stat-independent lease digest over one observed index state
/// (CB-8B ac-3).
///
/// The Git-index effect lease authenticates the index's semantic content —
/// sorted `(path, stage, mode, oid)` entries with their intent/skip/assume
/// flags — never the on-disk bytes, so a stat-cache refresh by an external
/// `git status` does not masquerade as a third lease value.
pub(super) fn index_lease_digest(entries: &[GitIndexEntry]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher_update_tag(&mut hasher, b"atomic:git-index-lease:v1\0");
    for entry in entries {
        hasher_update_tag(&mut hasher, entry.path.as_bytes());
        hasher_update_tag(&mut hasher, &entry.stage.to_be_bytes());
        hasher_update_tag(&mut hasher, &entry.mode.to_be_bytes());
        match &entry.oid {
            Some(oid) => hasher_update_tag(&mut hasher, oid.as_bytes()),
            None => hasher_update_tag(&mut hasher, b"\0absent"),
        }
        hasher_update_tag(&mut hasher, &[u8::from(entry.intent_to_add)]);
        hasher_update_tag(&mut hasher, &[u8::from(entry.skip_worktree)]);
        hasher_update_tag(&mut hasher, &[u8::from(entry.assume_unchanged)]);
        hasher_update_tag(&mut hasher, &[u8::from(entry.sparse_directory)]);
    }
    Hash::of(&hasher.finalize().as_bytes()[..])
}

fn hasher_update_tag(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Observe one open index as a core `GitIndexState` lease value (CB-8B ac-3).
pub(super) fn observe_index_lease(
    algorithm: GitHashAlgorithm,
    index: &git2::Index,
) -> Result<atomic_core::operation::GitIndexState, ObservationError> {
    let state = observe_open_git_index(algorithm, index)?;
    Ok(atomic_core::operation::GitIndexState {
        digest: index_lease_digest(&state.entries),
        tree: state.tree,
    })
}

/// Observe HEAD, index identity, locks, and Git-owned sequence state without mutation.
pub fn observe_git_metadata(root: &Path) -> Result<WorkspaceGitObservation, ObservationError> {
    let git_marker_exists = fs::symlink_metadata(root.join(".git")).is_ok();
    let repository = match git2::Repository::open(root) {
        Ok(repository) => repository,
        Err(error) if error.code() == git2::ErrorCode::NotFound && !git_marker_exists => {
            return Ok(WorkspaceGitObservation::NoGit {
                root: resolve_metadata_path(root, root),
            });
        }
        Err(error) => {
            return Err(ObservationError::GitOpen {
                path: root.display().to_string(),
                message: error.to_string(),
            });
        }
    };

    let worktree_git_dir = resolve_metadata_path(repository.path(), root);
    let common_dir = resolve_metadata_common_dir(&worktree_git_dir)?;
    let index = repository
        .index()
        .map_err(|error| ObservationError::GitIndex(error.to_string()))?;
    let index_path = index
        .path()
        .map(|path| resolve_metadata_path(path, &worktree_git_dir))
        .unwrap_or_else(|| worktree_git_dir.join("index"));
    let index_bytes = match fs::read(&index_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(ObservationError::GitIndex(format!(
                "cannot read '{}': {error}",
                index_path.display()
            )));
        }
    };
    let index_state = observe_open_git_index(repository_object_algorithm(&repository)?, &index)?;
    let mut index_stages: Vec<u8> = index_state
        .entries
        .iter()
        .map(|entry| entry.stage)
        .collect();
    index_stages.sort_unstable();
    index_stages.dedup();
    let index_tree = index_state.tree.as_ref().map(git_object_id_hex);

    let head = observe_metadata_head(&repository)?;
    let head_tree = match head_oid(&head) {
        Some(oid) => {
            let oid = git2::Oid::from_str(oid).map_err(|error| {
                ObservationError::GitIndex(format!("cannot parse observed Git HEAD: {error}"))
            })?;
            let commit = repository.find_commit(oid).map_err(|error| {
                ObservationError::GitIndex(format!("cannot read Git HEAD commit: {error}"))
            })?;
            Some(commit.tree_id().to_string())
        }
        None => None,
    };
    let index_lock = observe_metadata_path(append_metadata_suffix(&index_path, ".lock"))?;
    let markers = GitOperationMarker::ALL
        .iter()
        .copied()
        .map(|marker| {
            let path = worktree_git_dir.join(marker.relative_path());
            observe_metadata_path(path).map(|observation| GitOperationMarkerObservation {
                marker,
                path: observation.path,
                kind: observation.kind,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let operation = GitOperationObservation {
        repository_state: format!("{:?}", repository.state()),
        markers,
    };

    Ok(WorkspaceGitObservation::Repository(Box::new(
        WorkspaceGitRepositoryObservation {
            worktree_git_dir,
            common_dir,
            index_path,
            head,
            head_tree,
            index_digest: Hash::of(&index_bytes),
            index_tree,
            index_stages,
            index_lock,
            operation,
        },
    )))
}

fn observe_metadata_head(
    repository: &git2::Repository,
) -> Result<GitHeadObservation, ObservationError> {
    let head = repository
        .find_reference("HEAD")
        .map_err(|error| ObservationError::GitIndex(format!("cannot read Git HEAD: {error}")))?;
    if let Some(symref) = head.symbolic_target() {
        let symref = symref.to_string();
        match head.resolve() {
            Ok(resolved) => resolved
                .target()
                .map(|oid| GitHeadObservation::Attached {
                    symref,
                    oid: oid.to_string(),
                })
                .ok_or_else(|| {
                    ObservationError::GitIndex(
                        "Git HEAD target does not resolve directly to an object".to_string(),
                    )
                }),
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                let mut has_resolved_ref = false;
                for reference in repository
                    .references()
                    .map_err(|error| ObservationError::GitIndex(error.to_string()))?
                {
                    has_resolved_ref |= reference
                        .map_err(|error| ObservationError::GitIndex(error.to_string()))?
                        .target()
                        .is_some();
                }
                if has_resolved_ref {
                    Ok(GitHeadObservation::MissingTarget { symref })
                } else {
                    Ok(GitHeadObservation::Unborn { symref })
                }
            }
            Err(error) if error.code() == git2::ErrorCode::UnbornBranch => {
                Ok(GitHeadObservation::Unborn { symref })
            }
            Err(error) => Err(ObservationError::GitIndex(format!(
                "cannot resolve Git HEAD target '{symref}': {error}"
            ))),
        }
    } else if let Some(oid) = head.target() {
        Ok(GitHeadObservation::Detached {
            oid: oid.to_string(),
        })
    } else {
        Err(ObservationError::GitIndex(
            "Git HEAD is neither symbolic nor direct".to_string(),
        ))
    }
}

pub(super) fn git_object_id_hex(oid: &GitObjectId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(oid.as_bytes().len() * 2);
    for byte in oid.as_bytes() {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn head_oid(head: &GitHeadObservation) -> Option<&str> {
    match head {
        GitHeadObservation::Attached { oid, .. } | GitHeadObservation::Detached { oid } => {
            Some(oid)
        }
        GitHeadObservation::Unborn { .. } | GitHeadObservation::MissingTarget { .. } => None,
    }
}

fn observe_metadata_path(path: PathBuf) -> Result<GitAdminPathObservation, ObservationError> {
    let kind = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => GitAdminEntryKind::Symlink,
        Ok(metadata) if metadata.is_file() => GitAdminEntryKind::File,
        Ok(metadata) if metadata.is_dir() => GitAdminEntryKind::Directory,
        Ok(_) => GitAdminEntryKind::Other,
        Err(error) if error.kind() == io::ErrorKind::NotFound => GitAdminEntryKind::Missing,
        Err(error) => {
            return Err(ObservationError::WorktreeIo {
                path: path.display().to_string(),
                message: error.to_string(),
            });
        }
    };
    Ok(GitAdminPathObservation { path, kind })
}

fn resolve_metadata_common_dir(worktree_git_dir: &Path) -> Result<PathBuf, ObservationError> {
    let pointer = worktree_git_dir.join("commondir");
    let bytes = match fs::read(&pointer) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(worktree_git_dir.to_path_buf());
        }
        Err(error) => {
            return Err(ObservationError::WorktreeIo {
                path: pointer.display().to_string(),
                message: error.to_string(),
            });
        }
    };
    let value = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let value = value.strip_suffix(b"\r").unwrap_or(value);
    let value = std::str::from_utf8(value).map_err(|error| ObservationError::WorktreeIo {
        path: pointer.display().to_string(),
        message: error.to_string(),
    })?;
    Ok(resolve_metadata_path(Path::new(value), worktree_git_dir))
}

fn resolve_metadata_path(path: &Path, base: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    fs::canonicalize(&absolute).unwrap_or(absolute)
}

fn append_metadata_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

/// The physical form of the colocated `.git` path, observed fresh at
/// cutover time (CB-13B R6). Mere existence is never readiness: only
/// [`ColocatedGitForm::Repository`] proves an openable Git repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColocatedGitForm {
    /// No `.git` path at all.
    Absent,
    /// An openable Git repository (a `.git` directory or a worktree
    /// gitdir pointer file).
    Repository,
    /// A directory that is not a valid Git repository (e.g. empty).
    InvalidDirectory,
    /// A regular file that is not a valid Git gitdir pointer.
    InvalidFile,
    /// A symlink that does not resolve to a valid Git repository.
    InvalidSymlink,
}

/// The marker prefix identifying an Atomic-owned advisory dispatcher,
/// shared with the CLI hook installer (CB-13B R2): the cutover
/// decommissions these through journaled migration-effect leases while
/// foreign hooks refuse.
pub const ATOMIC_DISPATCHER_MARKER: &str = "# atomic:git-bridge-dispatcher:v1";
/// Legacy Atomic import-hook block marker (also Atomic-owned).
pub const ATOMIC_LEGACY_MARKER_BEGIN: &str = "# atomic:git:begin";

fn owned_dispatcher_content(bytes: &[u8]) -> bool {
    bytes.starts_with(format!("#!/bin/sh\n{ATOMIC_DISPATCHER_MARKER}\n").as_bytes())
        || bytes.starts_with(ATOMIC_LEGACY_MARKER_BEGIN.as_bytes())
}

/// An Atomic-owned advisory dispatcher observed in the colocated hooks
/// directory (CB-13B R2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedHookDispatcher {
    /// Hook file name (e.g. `post-checkout`).
    pub name: String,
    /// Path relative to the repository root, when the hooks directory
    /// lives under it (a linked worktree's common gitdir does not).
    pub path: Option<String>,
    /// Content hash of the dispatcher bytes (the effect lease value).
    pub content: Hash,
    /// Executable file mode.
    pub mode: u32,
}

/// Colocated Git readiness observed for the CB-13B cutover: the physical
/// form plus the hook and remote surfaces the audit labels `Refused`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColocatedGitReadiness {
    pub form: ColocatedGitForm,
    /// Active FOREIGN hook surfaces: a configured `core.hooksPath` and
    /// non-sample executable files that Atomic does not own.
    pub active_hooks: Vec<String>,
    /// Atomic-owned advisory dispatchers: the cutover decommissions
    /// these through journaled migration-effect leases.
    pub owned_dispatchers: Vec<OwnedHookDispatcher>,
    /// Configured remote names.
    pub remotes: Vec<String>,
    /// The repository's resolved HEAD (hex oid), when openable.
    pub head: Option<String>,
}

impl ColocatedGitReadiness {
    /// An absent observation (no colocated Git at all).
    fn absent() -> Self {
        Self {
            form: ColocatedGitForm::Absent,
            active_hooks: Vec::new(),
            owned_dispatchers: Vec::new(),
            remotes: Vec::new(),
            head: None,
        }
    }

    fn invalid(form: ColocatedGitForm) -> Self {
        Self {
            form,
            active_hooks: Vec::new(),
            owned_dispatchers: Vec::new(),
            remotes: Vec::new(),
            head: None,
        }
    }
}

/// Observe the colocated Git repository's physical form, hooks and
/// remotes without mutation (CB-13B R6). The observation never refuses by
/// itself: classification only; policy lives with the cutover gates.
pub fn observe_colocated_git_readiness(root: &Path) -> ColocatedGitReadiness {
    let metadata = match fs::symlink_metadata(root.join(".git")) {
        Ok(metadata) => metadata,
        Err(_) => return ColocatedGitReadiness::absent(),
    };
    let invalid_form = if metadata.file_type().is_symlink() {
        ColocatedGitForm::InvalidSymlink
    } else if metadata.is_dir() {
        ColocatedGitForm::InvalidDirectory
    } else {
        ColocatedGitForm::InvalidFile
    };
    let Ok(repository) = git2::Repository::open(root) else {
        return ColocatedGitReadiness::invalid(invalid_form);
    };

    let mut active_hooks = Vec::new();
    let mut owned_dispatchers = Vec::new();
    if let Ok(config) = repository.config() {
        if let Ok(hooks_path) = config.get_string("core.hooksPath") {
            active_hooks.push(format!("core.hooksPath={hooks_path}"));
        }
    }
    let hooks_dir = repository.path().join("hooks");
    if let Ok(entries) = fs::read_dir(&hooks_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".sample") || name.starts_with('.') {
                continue;
            }
            // Follow symlinks: a hook manager that symlinks a dispatcher
            // into hooks/ is an active hook surface, not an inert entry.
            let metadata = match fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(_) => {
                    // A dangling symlink in hooks/ is still an active
                    // surface: it names a hook the manager intends to run.
                    active_hooks.push(format!("hooks/{name} (broken symlink)"));
                    continue;
                }
            };
            let is_file = metadata.is_file();
            if !is_file {
                continue;
            }
            #[cfg(unix)]
            let (executable, mode) = {
                use std::os::unix::fs::PermissionsExt;
                // The effect lease compares against the observation's
                // permission-bit mode (no file-type bits).
                let mode = metadata.permissions().mode() & 0o777;
                (mode != 0, mode)
            };
            #[cfg(not(unix))]
            let (executable, mode) = (true, 0o755u32);
            if !executable {
                continue;
            }
            let bytes = fs::read(entry.path()).unwrap_or_default();
            if owned_dispatcher_content(&bytes) {
                // The decommission effect must address the file relative
                // to the worktree root; a linked worktree's common gitdir
                // is outside it and stays an explicit refusal.
                let path = entry
                    .path()
                    .strip_prefix(root)
                    .ok()
                    .map(|relative| relative.to_string_lossy().to_string());
                owned_dispatchers.push(OwnedHookDispatcher {
                    name,
                    path,
                    content: Hash::of(&bytes),
                    mode,
                });
            } else {
                active_hooks.push(format!("hooks/{name}"));
            }
        }
    }
    active_hooks.sort();
    owned_dispatchers.sort_by(|left, right| left.name.cmp(&right.name));
    let remotes = repository
        .remotes()
        .map(|names| {
            names
                .iter()
                .flatten()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let head = repository
        .head()
        .ok()
        .and_then(|head| head.target().map(|oid| oid.to_string()));
    ColocatedGitReadiness {
        form: ColocatedGitForm::Repository,
        active_hooks,
        owned_dispatchers,
        remotes,
        head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_tree_is_unavailable_for_conflicts_and_intent_to_add() {
        let oid = GitObjectId::new(GitHashAlgorithm::Sha1, vec![1; 20]).unwrap();
        let base = GitIndexEntry {
            path: RepoPath::from_bytes(b"file").unwrap(),
            stage: 2,
            mode: 0o100644,
            oid: Some(oid),
            intent_to_add: false,
            skip_worktree: false,
            assume_unchanged: false,
            sparse_directory: false,
        };
        assert!(
            compute_index_tree(GitHashAlgorithm::Sha1, std::slice::from_ref(&base))
                .unwrap()
                .is_none()
        );
        let mut intent = base;
        intent.stage = 0;
        intent.intent_to_add = true;
        assert!(compute_index_tree(GitHashAlgorithm::Sha1, &[intent])
            .unwrap()
            .is_none());
    }
}
