//! Canonical repository manifests and deterministic Atomic/Git tree projection.
//!
//! These types model repository state only. Index/worktree mutation, full stage
//! semantics, mismatch reporting, and publication enforcement belong to CB-4B.

use super::*;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use atomic_core::change::InodeKind;
use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_core::output::alive::RetrieveOptions;
use atomic_core::output::project_inode_attributes;

use atomic_core::types::SetId;
use atomic_objects::{content_key, ObjectKey};
use sha1::Digest as _;
use thiserror::Error;

pub const REPO_PATH_VERSION: u8 = 1;
pub const REPOSITORY_MANIFEST_VERSION: u8 = 1;
pub const GIT_INDEX_STATE_VERSION: u8 = 1;
pub const WORKTREE_OBSERVATION_VERSION: u8 = 1;
pub const CONVERSION_POLICY_VERSION: u8 = 1;

/// A validated repository-relative path represented as raw Unix bytes.
///
/// Ordering is bytewise over the canonical full path. Display escaping is
/// deliberately separate from identity.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(Vec<u8>);

impl RepoPath {
    pub fn new(bytes: Vec<u8>) -> Result<Self, ProjectTreeError> {
        validate_repo_path(&bytes)?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProjectTreeError> {
        Self::new(bytes.to_vec())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|byte| *byte == b'/')
    }

    pub fn file_name(&self) -> &[u8] {
        self.0.rsplit(|byte| *byte == b'/').next().unwrap_or(&[])
    }

    pub fn to_versioned_bytes(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.u8(REPO_PATH_VERSION);
        encoder.bytes(&self.0);
        encoder.finish()
    }

    pub fn from_versioned_bytes(bytes: &[u8]) -> Result<Self, ProjectTreeError> {
        let mut decoder = Decoder::new(bytes);
        decoder.version(REPO_PATH_VERSION, "RepoPath")?;
        let path = Self::new(decoder.bytes()?.to_vec())?;
        decoder.finish()?;
        Ok(path)
    }

    /// Convert a native path without normalization or replacement characters.
    #[cfg(unix)]
    pub fn from_native(path: &Path) -> Result<Self, ProjectTreeError> {
        use std::os::unix::ffi::OsStrExt;
        Self::from_bytes(path.as_os_str().as_bytes())
    }

    /// Native paths are refused where Rust cannot promise lossless Unix bytes.
    #[cfg(not(unix))]
    pub fn from_native(_path: &Path) -> Result<Self, ProjectTreeError> {
        Err(ProjectTreeError::UnsupportedPlatformPath)
    }

    #[cfg(unix)]
    pub fn to_native(&self) -> Result<PathBuf, ProjectTreeError> {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(self.0.clone())))
    }

    #[cfg(not(unix))]
    pub fn to_native(&self) -> Result<PathBuf, ProjectTreeError> {
        Err(ProjectTreeError::UnsupportedPlatformPath)
    }

    pub fn escaped(&self) -> String {
        escape_repo_path(&self.0)
    }

    /// Whether this path's canonical String identity needs escaping: any
    /// non-ASCII-graphic byte, or a literal `%` (which would make the escape
    /// scheme ambiguous). A plain printable-UTF-8 path without `%` passes
    /// through unchanged; everything else carries the injective `%XX`
    /// reversible form.
    pub fn needs_escaping(&self) -> bool {
        self.0
            .iter()
            .any(|byte| !byte.is_ascii_graphic() || *byte == b'%')
    }
}

/// The canonical reversible escape for repository path bytes (review CB-9C
/// R7): every byte that is not ASCII graphic — and every literal `%` — is
/// encoded as `%XX`. The mapping is injective over all byte strings and its
/// inverse is [`unescape_repo_path`], so raw non-UTF-8 path bytes survive
/// reversible display escaping; normalization never becomes identity.
pub fn escape_repo_path(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in bytes {
        if byte.is_ascii_graphic() && *byte != b'%' {
            output.push(char::from(*byte));
        } else {
            use std::fmt::Write;
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

/// The exact inverse of [`escape_repo_path`]: decode every `%XX` escape back
/// to its byte. Errors on a malformed escape (`%` not followed by two hex
/// digits), never silently rewriting identity.
pub fn unescape_repo_path(escaped: &str) -> Result<Vec<u8>, ProjectTreeError> {
    let bytes = escaped.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        let Some(first) = bytes.get(index + 1).copied() else {
            return Err(ProjectTreeError::InvalidPath(format!(
                "truncated escape in path display {escaped:?}"
            )));
        };
        let Some(second) = bytes.get(index + 2).copied() else {
            return Err(ProjectTreeError::InvalidPath(format!(
                "truncated escape in path display {escaped:?}"
            )));
        };
        let hex = |byte: u8| -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            }
        };
        let Some(high) = hex(first) else {
            return Err(ProjectTreeError::InvalidPath(format!(
                "invalid escape in path display {escaped:?}"
            )));
        };
        let Some(low) = hex(second) else {
            return Err(ProjectTreeError::InvalidPath(format!(
                "invalid escape in path display {escaped:?}"
            )));
        };
        output.push((high << 4) | low);
        index += 3;
    }
    Ok(output)
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RepoPath")
            .field(&self.escaped())
            .finish()
    }
}

fn validate_repo_path(bytes: &[u8]) -> Result<(), ProjectTreeError> {
    if bytes.is_empty() {
        return Err(ProjectTreeError::InvalidPath("path is empty".into()));
    }
    if bytes[0] == b'/' {
        return Err(ProjectTreeError::InvalidPath("path is absolute".into()));
    }
    if bytes.contains(&0) {
        return Err(ProjectTreeError::InvalidPath("path contains NUL".into()));
    }
    for component in bytes.split(|byte| *byte == b'/') {
        if component.is_empty() {
            return Err(ProjectTreeError::InvalidPath(
                "path contains an empty component".into(),
            ));
        }
        if component == b"." || component == b".." {
            return Err(ProjectTreeError::InvalidPath(
                "path contains '.' or '..'".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExclusionReason {
    AtomicPrivate,
    VaultPrivate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExclusionPolicy {
    BridgePrivate,
    IncludeVault,
}

impl ExclusionPolicy {
    pub fn exclusion(self, path: &RepoPath) -> Option<ExclusionReason> {
        if atomic_private_path(path.as_bytes()) {
            Some(ExclusionReason::AtomicPrivate)
        } else if path.components().next().unwrap_or_default() == b".vault"
            && self == Self::BridgePrivate
        {
            Some(ExclusionReason::VaultPrivate)
        } else {
            None
        }
    }
}

/// Whether `path` is Atomic-private bridge state (`.atomic*`): tracked alive
/// in the worktree's Atomic view but deliberately absent from project
/// manifests and from Git. Staged whole-tree verification (review R3) exempts
/// these paths from the extra-path refusal exactly as the conversion policy
/// does.
pub fn atomic_private_path(path_bytes: &[u8]) -> bool {
    let first = path_bytes
        .split(|byte| *byte == b'/')
        .next()
        .unwrap_or_default();
    first == b".atomic" || path_bytes == b".atomicignore"
}

/// Whether `path` is bridge-private state under
/// [`ExclusionPolicy::BridgePrivate`] — deliberately absent from project
/// manifests while tracked alive in the Atomic view. This is exactly the
/// path set [`ExclusionPolicy::BridgePrivate::exclusion`] rejects: Atomic
/// private state (`.atomic*`) plus the vault (`.vault/`), whose files a
/// colocated repository legitimately tracks in both Git and the graph.
/// Staged verification must exempt the whole set, not only the `.atomic*`
/// half, or importing a repository that tracks `.vault/` files refuses with
/// "staged path ... is present but the commit tree does not hold it".
pub fn bridge_private_path(path_bytes: &[u8]) -> bool {
    let first = path_bytes
        .split(|byte| *byte == b'/')
        .next()
        .unwrap_or_default();
    first == b".atomic" || path_bytes == b".atomicignore" || first == b".vault"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LossPolicy {
    Refuse,
    Record,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestDisposition {
    Included,
    Excluded(ExclusionReason),
}

/// One canonical repository entry. `repository_bytes` are already-clean bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryEntry {
    pub path: RepoPath,
    pub repository_bytes: Vec<u8>,
    pub content_identity: ObjectKey,
    pub mode: u16,
    pub kind: InodeKind,
    pub gitlink: Option<GitObjectId>,
    pub disposition: ManifestDisposition,
    pub losses: Vec<String>,
}

impl RepositoryEntry {
    pub fn new(
        path: RepoPath,
        repository_bytes: Vec<u8>,
        mode: u16,
        kind: InodeKind,
        gitlink: Option<GitObjectId>,
        disposition: ManifestDisposition,
    ) -> Result<Self, ProjectTreeError> {
        if mode > 0o777 {
            return Err(ProjectTreeError::InvalidMode(mode));
        }
        if (kind == InodeKind::Gitlink) != gitlink.is_some() {
            return Err(ProjectTreeError::InvalidGitlink(
                "gitlink kind and object identity must be present together".into(),
            ));
        }
        if let Some(oid) = &gitlink {
            if repository_bytes != git_oid_hex(oid) {
                return Err(ProjectTreeError::InvalidGitlink(
                    "repository bytes must be the lowercase hexadecimal object ID".into(),
                ));
            }
        }
        Ok(Self {
            path,
            content_identity: content_key(&repository_bytes),
            repository_bytes,
            mode,
            kind,
            gitlink,
            disposition,
            losses: Vec::new(),
        })
    }

    pub fn git_mode(&self) -> u32 {
        match self.kind {
            InodeKind::Regular if self.mode & 0o111 != 0 => 0o100755,
            InodeKind::Regular => 0o100644,
            InodeKind::Symlink => 0o120000,
            InodeKind::Gitlink => 0o160000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestRoot {
    pub version: u8,
    pub content_key: ObjectKey,
}

/// Versioned canonical full-tree repository projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryManifest {
    pub version: u8,
    pub set_id: SetId,
    pub conversion_policy_root: ObjectKey,
    pub entries: Vec<RepositoryEntry>,
}

impl RepositoryManifest {
    pub fn new(
        set_id: SetId,
        conversion_policy_root: ObjectKey,
        mut entries: Vec<RepositoryEntry>,
    ) -> Result<Self, ProjectTreeError> {
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        for pair in entries.windows(2) {
            if pair[0].path == pair[1].path {
                return Err(ProjectTreeError::DuplicatePath(pair[0].path.clone()));
            }
        }
        Ok(Self {
            version: REPOSITORY_MANIFEST_VERSION,
            set_id,
            conversion_policy_root,
            entries,
        })
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.tag(b"atomic.repository-manifest");
        encoder.u8(self.version);
        encoder.raw(self.set_id.as_bytes());
        encoder.bytes(self.conversion_policy_root.as_bytes());
        encoder.u32(self.entries.len() as u32);
        for entry in &self.entries {
            encoder.bytes(entry.path.as_bytes());
            encoder.u8(entry.kind.as_byte());
            encoder.u16(entry.mode);
            encoder.u8(match entry.disposition {
                ManifestDisposition::Included => 0,
                ManifestDisposition::Excluded(ExclusionReason::AtomicPrivate) => 1,
                ManifestDisposition::Excluded(ExclusionReason::VaultPrivate) => 2,
            });
            encoder.bytes(entry.content_identity.as_bytes());
            encoder.bytes(&entry.repository_bytes);
            encoder.git_oid(entry.gitlink.as_ref());
            encoder.u32(entry.losses.len() as u32);
            for loss in &entry.losses {
                encoder.bytes(loss.as_bytes());
            }
        }
        encoder.finish()
    }

    pub fn root(&self) -> ManifestRoot {
        ManifestRoot {
            version: self.version,
            content_key: content_key(&self.canonical_bytes()),
        }
    }

    pub fn included_entries(&self) -> impl Iterator<Item = &RepositoryEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.disposition == ManifestDisposition::Included)
    }
}

/// Platform facts included in the conversion-policy fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformCapabilities {
    pub lossless_unix_paths: bool,
    pub symlinks: bool,
    pub executable_bit: bool,
    pub case_sensitive: bool,
    pub unicode_normalizing: bool,
}

/// Versioned policy whose root invalidates projected-tree caches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversionPolicy {
    pub version: u8,
    pub attributes_tree: Option<GitObjectId>,
    pub relevant_git_config: ObjectKey,
    pub filter_driver_versions: Vec<ObjectKey>,
    pub platform: PlatformCapabilities,
    pub object_format: GitHashAlgorithm,
    pub exclusions: ExclusionPolicy,
    pub losses: LossPolicy,
}

impl ConversionPolicy {
    pub fn new(object_format: GitHashAlgorithm) -> Self {
        Self {
            version: CONVERSION_POLICY_VERSION,
            attributes_tree: None,
            relevant_git_config: content_key(&[]),
            filter_driver_versions: Vec::new(),
            platform: PlatformCapabilities {
                lossless_unix_paths: cfg!(unix),
                symlinks: cfg!(unix),
                executable_bit: cfg!(unix),
                case_sensitive: cfg!(unix),
                unicode_normalizing: false,
            },
            object_format,
            exclusions: ExclusionPolicy::BridgePrivate,
            losses: LossPolicy::Refuse,
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.tag(b"atomic.conversion-policy");
        encoder.u8(self.version);
        encoder.git_oid(self.attributes_tree.as_ref());
        encoder.bytes(self.relevant_git_config.as_bytes());
        let mut drivers = self.filter_driver_versions.clone();
        drivers.sort();
        encoder.u32(drivers.len() as u32);
        for driver in drivers {
            encoder.bytes(driver.as_bytes());
        }
        encoder.u8(self.platform.lossless_unix_paths as u8);
        encoder.u8(self.platform.symlinks as u8);
        encoder.u8(self.platform.executable_bit as u8);
        encoder.u8(self.platform.case_sensitive as u8);
        encoder.u8(self.platform.unicode_normalizing as u8);
        encoder.u8(algorithm_tag(self.object_format));
        encoder.u8(match self.exclusions {
            ExclusionPolicy::BridgePrivate => 0,
            ExclusionPolicy::IncludeVault => 1,
        });
        encoder.u8(match self.losses {
            LossPolicy::Refuse => 0,
            LossPolicy::Record => 1,
        });
        encoder.finish()
    }

    pub fn root(&self) -> ManifestRoot {
        ManifestRoot {
            version: self.version,
            content_key: content_key(&self.canonical_bytes()),
        }
    }
}

/// A stage-aware index entry model used as a CB-4A input contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitIndexEntry {
    pub path: RepoPath,
    pub stage: u8,
    pub mode: u32,
    pub oid: Option<GitObjectId>,
    pub intent_to_add: bool,
    pub skip_worktree: bool,
    pub assume_unchanged: bool,
    pub sparse_directory: bool,
}

/// Explicit versioned index state. CB-4A does not mutate or fully interpret it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitIndexState {
    pub version: u8,
    pub index_version: u32,
    pub object_format: GitHashAlgorithm,
    pub entries: Vec<GitIndexEntry>,
    /// Exact tree represented by ordinary stage-0 entries, when one exists.
    pub tree: Option<GitObjectId>,
    /// The on-disk index uses sparse-directory (`sdir`) entries and was
    /// observed through read-only in-memory expansion. Sparse absence is
    /// never a deletion.
    pub sparse_index: bool,
}

impl GitIndexState {
    pub fn new(object_format: GitHashAlgorithm, entries: Vec<GitIndexEntry>) -> Self {
        Self {
            version: GIT_INDEX_STATE_VERSION,
            index_version: 2,
            object_format,
            entries,
            tree: None,
            sparse_index: false,
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut entries = self.entries.clone();
        entries.sort_by(|left, right| (&left.path, left.stage).cmp(&(&right.path, right.stage)));
        let mut encoder = Encoder::new();
        encoder.tag(b"atomic.git-index-state");
        encoder.u8(self.version);
        encoder.u32(self.index_version);
        encoder.u8(algorithm_tag(self.object_format));
        encoder.git_oid(self.tree.as_ref());
        encoder.u32(entries.len() as u32);
        for entry in entries {
            encoder.bytes(entry.path.as_bytes());
            encoder.u8(entry.stage);
            encoder.u32(entry.mode);
            encoder.git_oid(entry.oid.as_ref());
            encoder.u8(entry.intent_to_add as u8);
            encoder.u8(entry.skip_worktree as u8);
            encoder.u8(entry.assume_unchanged as u8);
            encoder.u8(entry.sparse_directory as u8);
        }
        encoder.finish()
    }

    pub fn root(&self) -> ManifestRoot {
        ManifestRoot {
            version: self.version,
            content_key: content_key(&self.canonical_bytes()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: RepoPath,
    pub physical_kind: PhysicalKind,
    /// Repository kind supplied by the stage-0 index, when available.
    pub repository_kind: Option<InodeKind>,
    pub gitlink: Option<GitObjectId>,
    pub mode: Option<u16>,
    pub size: u64,
    pub worktree_bytes: Vec<u8>,
    pub worktree_content: ObjectKey,
    pub repository_bytes_after_clean: Option<Vec<u8>>,
    pub repository_content_after_clean: Option<ObjectKey>,
    pub disposition: ManifestDisposition,
    pub filter_warnings: Vec<String>,
    pub filter_error: Option<String>,
}

/// Explicit versioned physical observation input for later CB-4B comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeObservation {
    pub version: u8,
    pub platform: PlatformCapabilities,
    pub entries: Vec<WorktreeEntry>,
}

impl WorktreeObservation {
    pub fn new(platform: PlatformCapabilities, entries: Vec<WorktreeEntry>) -> Self {
        Self {
            version: WORKTREE_OBSERVATION_VERSION,
            platform,
            entries,
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut entries = self.entries.clone();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let mut encoder = Encoder::new();
        encoder.tag(b"atomic.worktree-observation");
        encoder.u8(self.version);
        encoder.u8(self.platform.lossless_unix_paths as u8);
        encoder.u8(self.platform.symlinks as u8);
        encoder.u8(self.platform.executable_bit as u8);
        encoder.u8(self.platform.case_sensitive as u8);
        encoder.u8(self.platform.unicode_normalizing as u8);
        encoder.u32(entries.len() as u32);
        for entry in entries {
            encoder.bytes(entry.path.as_bytes());
            encoder.u8(match entry.physical_kind {
                PhysicalKind::Regular => 0,
                PhysicalKind::Directory => 1,
                PhysicalKind::Symlink => 2,
                PhysicalKind::Other => 3,
            });
            encoder.u8(entry.repository_kind.map_or(u8::MAX, InodeKind::as_byte));
            encoder.git_oid(entry.gitlink.as_ref());
            encoder.u16(entry.mode.unwrap_or(u16::MAX));
            encoder.u64(entry.size);
            encoder.bytes(&entry.worktree_bytes);
            encoder.bytes(entry.worktree_content.as_bytes());
            encoder.optional_bytes(entry.repository_bytes_after_clean.as_deref());
            encoder.optional_bytes(
                entry
                    .repository_content_after_clean
                    .as_deref()
                    .map(str::as_bytes),
            );
            encoder.u8(match entry.disposition {
                ManifestDisposition::Included => 0,
                ManifestDisposition::Excluded(ExclusionReason::AtomicPrivate) => 1,
                ManifestDisposition::Excluded(ExclusionReason::VaultPrivate) => 2,
            });
            encoder.u32(entry.filter_warnings.len() as u32);
            for warning in &entry.filter_warnings {
                encoder.bytes(warning.as_bytes());
            }
            encoder.optional_bytes(entry.filter_error.as_deref().map(str::as_bytes));
        }
        encoder.finish()
    }

    pub fn root(&self) -> ManifestRoot {
        ManifestRoot {
            version: self.version,
            content_key: content_key(&self.canonical_bytes()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitObjectKind {
    Blob,
    Tree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitObject {
    pub kind: GitObjectKind,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitTreeEntry {
    pub mode: u32,
    pub name: Vec<u8>,
    pub oid: GitObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitTree {
    pub algorithm: GitHashAlgorithm,
    pub root: GitObjectId,
    pub objects: GitObjectDatabase,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitObjectDatabase {
    objects: BTreeMap<GitObjectId, GitObject>,
}

impl GitObjectDatabase {
    pub fn get(&self, oid: &GitObjectId) -> Option<&GitObject> {
        self.objects.get(oid)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&GitObjectId, &GitObject)> {
        self.objects.iter()
    }

    /// Add an externally loaded Git object after verifying its tagged identity.
    pub fn insert_object(
        &mut self,
        oid: GitObjectId,
        object: GitObject,
    ) -> Result<(), ProjectTreeError> {
        let computed = git_object_id(oid.algorithm(), object.kind, &object.bytes)?;
        if computed != oid {
            return Err(ProjectTreeError::MalformedGitTree(
                "Git object bytes do not match the supplied object identity".into(),
            ));
        }
        self.objects.insert(oid, object);
        Ok(())
    }

    pub fn insert(
        &mut self,
        algorithm: GitHashAlgorithm,
        kind: GitObjectKind,
        bytes: Vec<u8>,
    ) -> Result<GitObjectId, ProjectTreeError> {
        let oid = git_object_id(algorithm, kind, &bytes)?;
        self.objects.insert(oid.clone(), GitObject { kind, bytes });
        Ok(oid)
    }

    pub fn parse_manifest(
        &self,
        root: &GitObjectId,
        policy: &ConversionPolicy,
    ) -> Result<RepositoryManifest, ProjectTreeError> {
        if root.algorithm() != policy.object_format {
            return Err(ProjectTreeError::ObjectAlgorithmMismatch);
        }
        let mut entries = Vec::new();
        self.parse_tree(root, &[], policy, &mut entries, &mut BTreeSet::new())?;
        RepositoryManifest::new(SetId::ZERO, policy.root().content_key, entries)
    }

    fn parse_tree(
        &self,
        oid: &GitObjectId,
        prefix: &[u8],
        policy: &ConversionPolicy,
        output: &mut Vec<RepositoryEntry>,
        visiting: &mut BTreeSet<GitObjectId>,
    ) -> Result<(), ProjectTreeError> {
        if !visiting.insert(oid.clone()) {
            return Err(ProjectTreeError::MalformedGitTree("tree cycle".into()));
        }
        let object = self
            .objects
            .get(oid)
            .ok_or_else(|| ProjectTreeError::MissingGitObject(oid.clone()))?;
        if object.kind != GitObjectKind::Tree
            || git_object_id(oid.algorithm(), object.kind, &object.bytes)? != *oid
        {
            return Err(ProjectTreeError::MalformedGitTree(
                "tree object identity or kind mismatch".into(),
            ));
        }
        let parsed = parse_tree_entries(&object.bytes, oid.algorithm())?;
        let mut previous: Option<(Vec<u8>, bool)> = None;
        for tree_entry in parsed {
            let is_tree = tree_entry.mode == 0o040000;
            if let Some((name, previous_tree)) = &previous {
                if git_tree_name_cmp(name, *previous_tree, &tree_entry.name, is_tree)
                    != Ordering::Less
                {
                    return Err(ProjectTreeError::MalformedGitTree(
                        "entries are not in canonical Git order".into(),
                    ));
                }
            }
            previous = Some((tree_entry.name.clone(), is_tree));
            let mut path = prefix.to_vec();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&tree_entry.name);
            if is_tree {
                self.parse_tree(&tree_entry.oid, &path, policy, output, visiting)?;
                continue;
            }
            let path = RepoPath::new(path)?;
            let disposition = policy
                .exclusions
                .exclusion(&path)
                .map(ManifestDisposition::Excluded)
                .unwrap_or(ManifestDisposition::Included);
            let (kind, mode, bytes, gitlink) = match tree_entry.mode {
                0o100644 => (
                    InodeKind::Regular,
                    0o644,
                    self.verified_blob(&tree_entry.oid)?,
                    None,
                ),
                0o100755 => (
                    InodeKind::Regular,
                    0o755,
                    self.verified_blob(&tree_entry.oid)?,
                    None,
                ),
                0o120000 => (
                    InodeKind::Symlink,
                    0o777,
                    self.verified_blob(&tree_entry.oid)?,
                    None,
                ),
                0o160000 => {
                    let bytes = git_oid_hex(&tree_entry.oid);
                    (InodeKind::Gitlink, 0o644, bytes, Some(tree_entry.oid))
                }
                mode => return Err(ProjectTreeError::UnsupportedGitMode(mode)),
            };
            output.push(RepositoryEntry::new(
                path,
                bytes,
                mode,
                kind,
                gitlink,
                disposition,
            )?);
        }
        visiting.remove(oid);
        Ok(())
    }

    fn verified_blob(&self, oid: &GitObjectId) -> Result<Vec<u8>, ProjectTreeError> {
        let object = self
            .objects
            .get(oid)
            .ok_or_else(|| ProjectTreeError::MissingGitObject(oid.clone()))?;
        if object.kind != GitObjectKind::Blob
            || git_object_id(oid.algorithm(), object.kind, &object.bytes)? != *oid
        {
            return Err(ProjectTreeError::MalformedGitTree(
                "blob object identity or kind mismatch".into(),
            ));
        }
        Ok(object.bytes.clone())
    }
}

/// Atomic projection result: canonical manifest and the equivalent Git tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTree {
    pub manifest: RepositoryManifest,
    pub git: GitTree,
}

impl ProjectTree {
    pub fn from_manifest(
        manifest: RepositoryManifest,
        policy: &ConversionPolicy,
    ) -> Result<Self, ProjectTreeError> {
        if manifest.version != REPOSITORY_MANIFEST_VERSION {
            return Err(ProjectTreeError::UnsupportedVersion {
                model: "RepositoryManifest",
                found: manifest.version,
                expected: REPOSITORY_MANIFEST_VERSION,
            });
        }
        if manifest.conversion_policy_root != policy.root().content_key {
            return Err(ProjectTreeError::PolicyRootMismatch);
        }
        let git = build_git_tree(&manifest, policy.object_format)?;
        Ok(Self { manifest, git })
    }
}

#[derive(Default)]
struct DirectoryNode {
    files: BTreeMap<Vec<u8>, RepositoryEntry>,
    directories: BTreeMap<Vec<u8>, DirectoryNode>,
}

fn build_git_tree(
    manifest: &RepositoryManifest,
    algorithm: GitHashAlgorithm,
) -> Result<GitTree, ProjectTreeError> {
    let mut root_node = DirectoryNode::default();
    for entry in manifest.included_entries() {
        let components: Vec<_> = entry.path.components().collect();
        let (name, parents) = components
            .split_last()
            .expect("validated path has a component");
        let mut directory = &mut root_node;
        for parent in parents {
            if directory.files.contains_key(*parent) {
                return Err(ProjectTreeError::PathCollision(entry.path.clone()));
            }
            directory = directory.directories.entry(parent.to_vec()).or_default();
        }
        if directory.directories.contains_key(*name)
            || directory
                .files
                .insert(name.to_vec(), entry.clone())
                .is_some()
        {
            return Err(ProjectTreeError::PathCollision(entry.path.clone()));
        }
    }
    let mut objects = GitObjectDatabase::default();
    let root = write_directory(&root_node, algorithm, &mut objects)?;
    Ok(GitTree {
        algorithm,
        root,
        objects,
    })
}

fn write_directory(
    directory: &DirectoryNode,
    algorithm: GitHashAlgorithm,
    objects: &mut GitObjectDatabase,
) -> Result<GitObjectId, ProjectTreeError> {
    let mut entries = Vec::new();
    for (name, child) in &directory.directories {
        let oid = write_directory(child, algorithm, objects)?;
        entries.push(GitTreeEntry {
            mode: 0o040000,
            // CB-9C path fidelity (review R7): manifest paths carry the
            // reversible escaped identity; Git tree objects hold the exact
            // raw bytes, so the escaped form decodes before writing.
            name: unescape_repo_path(&String::from_utf8_lossy(name))?,
            oid,
        });
    }
    for (name, entry) in &directory.files {
        let oid = match entry.kind {
            InodeKind::Gitlink => entry
                .gitlink
                .clone()
                .ok_or_else(|| ProjectTreeError::InvalidGitlink("missing object ID".into()))?,
            InodeKind::Regular | InodeKind::Symlink => objects.insert(
                algorithm,
                GitObjectKind::Blob,
                entry.repository_bytes.clone(),
            )?,
        };
        if oid.algorithm() != algorithm {
            return Err(ProjectTreeError::ObjectAlgorithmMismatch);
        }
        entries.push(GitTreeEntry {
            mode: entry.git_mode(),
            name: unescape_repo_path(&String::from_utf8_lossy(name))?,
            oid,
        });
    }
    entries.sort_by(|left, right| {
        git_tree_name_cmp(
            &left.name,
            left.mode == 0o040000,
            &right.name,
            right.mode == 0o040000,
        )
    });
    let mut bytes = Vec::new();
    for entry in entries {
        bytes.extend_from_slice(format!("{:o} ", entry.mode).as_bytes());
        bytes.extend_from_slice(&entry.name);
        bytes.push(0);
        bytes.extend_from_slice(entry.oid.as_bytes());
    }
    objects.insert(algorithm, GitObjectKind::Tree, bytes)
}

fn git_tree_name_cmp(left: &[u8], left_tree: bool, right: &[u8], right_tree: bool) -> Ordering {
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

fn parse_tree_entries(
    bytes: &[u8],
    algorithm: GitHashAlgorithm,
) -> Result<Vec<GitTreeEntry>, ProjectTreeError> {
    let oid_len = match algorithm {
        GitHashAlgorithm::Sha1 => 20,
        GitHashAlgorithm::Sha256 => 32,
    };
    let mut cursor = 0usize;
    let mut entries = Vec::new();
    while cursor < bytes.len() {
        let space = bytes[cursor..]
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| ProjectTreeError::MalformedGitTree("missing mode separator".into()))?
            + cursor;
        let mode_text = std::str::from_utf8(&bytes[cursor..space])
            .map_err(|_| ProjectTreeError::MalformedGitTree("non-ASCII mode".into()))?;
        if mode_text.starts_with('0') {
            return Err(ProjectTreeError::MalformedGitTree(
                "non-canonical mode".into(),
            ));
        }
        let mode = u32::from_str_radix(mode_text, 8)
            .map_err(|_| ProjectTreeError::MalformedGitTree("invalid mode".into()))?;
        cursor = space + 1;
        let nul = bytes[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| ProjectTreeError::MalformedGitTree("missing name terminator".into()))?
            + cursor;
        let name = bytes[cursor..nul].to_vec();
        if name.is_empty() || name.contains(&b'/') {
            return Err(ProjectTreeError::MalformedGitTree(
                "invalid tree entry name".into(),
            ));
        }
        cursor = nul + 1;
        let end = cursor
            .checked_add(oid_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| ProjectTreeError::MalformedGitTree("truncated object ID".into()))?;
        let oid = GitObjectId::new(algorithm, bytes[cursor..end].to_vec())
            .map_err(|error| ProjectTreeError::GitObjectId(error.to_string()))?;
        cursor = end;
        entries.push(GitTreeEntry { mode, name, oid });
    }
    Ok(entries)
}

pub(super) fn git_object_id(
    algorithm: GitHashAlgorithm,
    kind: GitObjectKind,
    bytes: &[u8],
) -> Result<GitObjectId, ProjectTreeError> {
    let kind = match kind {
        GitObjectKind::Blob => "blob",
        GitObjectKind::Tree => "tree",
    };
    let header = format!("{kind} {}\0", bytes.len());
    let digest = match algorithm {
        GitHashAlgorithm::Sha1 => {
            let mut hasher = sha1::Sha1::new();
            hasher.update(header.as_bytes());
            hasher.update(bytes);
            hasher.finalize().to_vec()
        }
        GitHashAlgorithm::Sha256 => {
            let mut hasher = sha2::Sha256::new();
            hasher.update(header.as_bytes());
            hasher.update(bytes);
            hasher.finalize().to_vec()
        }
    };
    GitObjectId::new(algorithm, digest)
        .map_err(|error| ProjectTreeError::GitObjectId(error.to_string()))
}

impl Repository {
    /// Build the canonical repository manifest and Git tree directly from graph state.
    /// Stored graph bytes are used verbatim; no smudge filter or working-copy read occurs.
    pub fn project_tree(
        &self,
        view_name: &str,
        policy: &ConversionPolicy,
    ) -> Result<ProjectTree, ProjectTreeError> {
        if !policy.platform.lossless_unix_paths {
            return Err(ProjectTreeError::UnsupportedPlatformPath);
        }
        let txn = self.pristine.read_txn().map_err(repo_error)?;
        let debug_tree = std::env::var("ATOMIC_DEBUG_PROJECTION").is_ok();
        let tree_start = std::time::Instant::now();
        let view = txn
            .get_view(view_name)
            .map_err(repo_error)?
            .ok_or_else(|| ProjectTreeError::Repository(format!("view '{view_name}' not found")))?;
        let closure = effective_projection_closure(&txn, &view).map_err(ProjectTreeError::from)?;
        if debug_tree {
            eprintln!("PTREE closure ms={}", tree_start.elapsed().as_millis());
        }
        let identity =
            effective_projection_identity(&txn, &view).map_err(ProjectTreeError::from)?;
        if debug_tree {
            eprintln!("PTREE identity ms={}", tree_start.elapsed().as_millis());
        }
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            closure.graph_visibility(),
        )
        .map_err(ProjectTreeError::from)?;
        if debug_tree {
            eprintln!(
                "PTREE claim_visibility ms={}",
                tree_start.elapsed().as_millis()
            );
        }
        let projection = self
            .project_tree_for_visibility(&txn, &claim_visibility)
            .map_err(ProjectTreeError::from)?;
        if debug_tree {
            eprintln!(
                "PTREE projection ms={} present={}",
                tree_start.elapsed().as_millis(),
                projection.present.len()
            );
        }
        if !projection.name_conflicts.is_empty() {
            let mut paths: Vec<_> = projection.name_conflicts.keys().cloned().collect();
            paths.sort();
            return Err(ProjectTreeError::Repository(format!(
                "unresolved path claims: {}",
                paths.join(", ")
            )));
        }
        let mut entries = Vec::new();
        for item in projection
            .present
            .values()
            .filter(|item| !item.is_directory)
        {
            let path = RepoPath::from_bytes(item.path.as_bytes())?;
            let disposition = policy
                .exclusions
                .exclusion(&path)
                .map(ManifestDisposition::Excluded)
                .unwrap_or(ManifestDisposition::Included);
            let attrs_start = std::time::Instant::now();
            let attrs =
                project_inode_attributes(&txn, item.position, closure.attribute_visibility())
                    .map_err(repo_error)?;
            if debug_tree && attrs_start.elapsed().as_millis() > 100 {
                eprintln!(
                    "PTREE attrs slow path={} ms={}",
                    path.escaped(),
                    attrs_start.elapsed().as_millis()
                );
            }
            if attrs.is_conflicted() {
                return Err(ProjectTreeError::Repository(format!(
                    "inode attributes conflict at '{}'",
                    path.escaped()
                )));
            }
            let materialization = attrs.materialization;
            let bytes = super::content::retrieve_content_with_filter_fast(
                &txn,
                &self.change_store,
                item.inode,
                item.position,
                RetrieveOptions::new().with_graph_visibility(closure.clone()),
            )
            .map_err(|error| ProjectTreeError::Repository(error.to_string()))?;
            let gitlink = if materialization.kind == InodeKind::Gitlink {
                Some(parse_gitlink_bytes(policy.object_format, &bytes)?)
            } else {
                None
            };
            entries.push(RepositoryEntry::new(
                path,
                bytes,
                materialization.mode,
                materialization.kind,
                gitlink,
                disposition,
            )?);
        }
        let manifest =
            RepositoryManifest::new(identity.set_id, policy.root().content_key, entries)?;
        ProjectTree::from_manifest(manifest, policy)
    }

    /// Project a historical state of a view without mutating or materializing it.
    pub fn project_tree_at_state(
        &self,
        view_name: &str,
        state: Merkle,
        policy: &ConversionPolicy,
    ) -> Result<ProjectTree, ProjectTreeError> {
        let txn = self.pristine.read_txn().map_err(repo_error)?;
        let view = txn
            .get_view(view_name)
            .map_err(repo_error)?
            .ok_or_else(|| ProjectTreeError::Repository(format!("view '{view_name}' not found")))?;
        let mut max_sequence = None;
        for row in txn.iter_changes(&view, 0).map_err(repo_error)? {
            let (sequence, _change, candidate_state) = row.map_err(repo_error)?;
            if candidate_state == state {
                max_sequence = Some(sequence + 1);
                break;
            }
        }
        let max_sequence = max_sequence.ok_or_else(|| {
            ProjectTreeError::Repository(format!(
                "state {} is not present in view '{view_name}'",
                state
            ))
        })?;
        let membership = super::view_membership_at_sequence(&txn, &view, max_sequence)
            .map_err(ProjectTreeError::from)?;
        let mut roots = Vec::with_capacity(membership.len());
        for change_id in membership.iter() {
            roots.push(
                txn.get_external(*change_id)
                    .map_err(repo_error)?
                    .ok_or_else(|| {
                        ProjectTreeError::Repository(format!(
                            "visible change {} has no external hash",
                            change_id.get()
                        ))
                    })?,
            );
        }
        drop(txn);
        self.project_tree_for_change_closure(view_name, &roots, policy)
    }

    /// Project the complete tree represented by explicit Atomic change roots and
    /// their validated dependency closure, without creating or mutating a view.
    pub fn project_tree_for_change_closure(
        &self,
        view_name: &str,
        roots: &[Hash],
        policy: &ConversionPolicy,
    ) -> Result<ProjectTree, ProjectTreeError> {
        if !policy.platform.lossless_unix_paths {
            return Err(ProjectTreeError::UnsupportedPlatformPath);
        }
        let txn = self.pristine.read_txn().map_err(repo_error)?;
        let view = txn
            .get_view(view_name)
            .map_err(repo_error)?
            .ok_or_else(|| ProjectTreeError::Repository(format!("view '{view_name}' not found")))?;
        self.project_change_closure_with_txn(&txn, &view, roots, policy)
    }

    /// The closure projection computed over an explicit transaction.
    ///
    /// Identical to [`Self::project_tree_for_change_closure`], but usable
    /// with a write transaction so the CB-6C resurrection builder can prove
    /// the projected tree against a binding inside the same isolated write
    /// transaction that applied the closure — before any membership is
    /// published. The manifest carries the recomputed order-invariant SetId
    /// of the closure.
    pub(super) fn project_change_closure_with_txn<T>(
        &self,
        txn: &T,
        view: &atomic_core::pristine::ViewState,
        roots: &[Hash],
        policy: &ConversionPolicy,
    ) -> Result<ProjectTree, ProjectTreeError>
    where
        T: atomic_core::pristine::ViewTxnT
            + atomic_core::pristine::GraphTxnT
            + atomic_core::pristine::TreeTxnT
            + atomic_core::pristine::PathClaimTxnT
            + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>
            + atomic_core::pristine::InodeAttrTxnT,
    {
        self.project_change_closure_with_conflict_markers(
            txn,
            view,
            roots,
            policy,
            &Default::default(),
        )
    }

    /// Project a change closure, substituting conflict-marker bytes for the
    /// named paths (RFC §8.3 conflict snapshot commits, CB-8B).
    ///
    /// Name-conflict paths are carried with their rendered marker bytes and
    /// injected even though path-claim resolution hides their sides; every
    /// other path behaves exactly like [`Self::project_change_closure_with_txn`].
    /// Attribute conflicts still refuse: markers cannot carry two modes.
    pub(super) fn project_change_closure_with_conflict_markers<T>(
        &self,
        txn: &T,
        view: &atomic_core::pristine::ViewState,
        roots: &[Hash],
        policy: &ConversionPolicy,
        marker_bytes: &std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Result<ProjectTree, ProjectTreeError>
    where
        T: atomic_core::pristine::ViewTxnT
            + atomic_core::pristine::GraphTxnT
            + atomic_core::pristine::TreeTxnT
            + atomic_core::pristine::PathClaimTxnT
            + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>
            + atomic_core::pristine::InodeAttrTxnT,
    {
        use atomic_core::pristine::{EffectiveProjectionClosure, ViewMembershipSet};

        let mut root_ids = Vec::with_capacity(roots.len());
        for hash in roots {
            root_ids.push(txn.get_internal(hash).map_err(repo_error)?.ok_or_else(|| {
                ProjectTreeError::Repository(format!("change {hash} is not registered"))
            })?);
        }
        let membership = ViewMembershipSet::from_ordered(root_ids);
        let closure = EffectiveProjectionClosure::try_from_membership(txn, &membership)
            .map_err(repo_error)?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            txn,
            &self.change_store,
            view,
            closure.graph_visibility(),
        )
        .map_err(ProjectTreeError::from)?;
        let projection = self
            .project_tree_for_visibility(txn, &claim_visibility)
            .map_err(ProjectTreeError::from)?;
        // Unprojected name claims refuse exactly like the clean path: a path
        // claimed by several inodes is carried only when its complete marker
        // bytes were captured (RFC §8.3); silently choosing a side is never
        // allowed.
        let mut unresolved_claims: Vec<&String> = projection
            .name_conflicts
            .keys()
            .filter(|path| !marker_bytes.contains_key(*path))
            .collect();
        unresolved_claims.sort();
        if !unresolved_claims.is_empty() {
            let paths: Vec<String> = unresolved_claims.into_iter().cloned().collect();
            return Err(ProjectTreeError::Repository(format!(
                "unresolved path claims: {}",
                paths.join(", ")
            )));
        }
        let mut entries = Vec::new();
        let mut projected_paths = std::collections::BTreeSet::new();
        for item in projection
            .present
            .values()
            .filter(|item| !item.is_directory)
        {
            let path = RepoPath::from_bytes(item.path.as_bytes())?;
            let disposition = policy
                .exclusions
                .exclusion(&path)
                .map(ManifestDisposition::Excluded)
                .unwrap_or(ManifestDisposition::Included);
            let attrs =
                project_inode_attributes(txn, item.position, closure.attribute_visibility())
                    .map_err(repo_error)?;
            if attrs.is_conflicted() {
                return Err(ProjectTreeError::Repository(format!(
                    "inode attributes conflict at '{}'",
                    path.escaped()
                )));
            }
            let materialization = attrs.materialization;
            let bytes = match marker_bytes.get(item.path.as_str()) {
                Some(override_bytes) => override_bytes.clone(),
                None => super::content::retrieve_content_with_filter_fast(
                    txn,
                    &self.change_store,
                    item.inode,
                    item.position,
                    RetrieveOptions::new().with_graph_visibility(closure.clone()),
                )
                .map_err(|error| ProjectTreeError::Repository(error.to_string()))?,
            };
            let gitlink = if materialization.kind == InodeKind::Gitlink && marker_bytes.is_empty() {
                Some(parse_gitlink_bytes(policy.object_format, &bytes)?)
            } else {
                None
            };
            projected_paths.insert(item.path.clone());
            entries.push(RepositoryEntry::new(
                path,
                bytes,
                materialization.mode,
                materialization.kind,
                gitlink,
                disposition,
            )?);
        }
        // Name-conflict paths are resolved away by path-claim visibility, so
        // they may be absent from `present`; their marker bytes are injected
        // as regular-file entries so the snapshot commit carries every side.
        for (path, bytes) in marker_bytes {
            if projected_paths.contains(path.as_str()) {
                continue;
            }
            let repo_path = RepoPath::from_bytes(path.as_bytes())?;
            let disposition = policy
                .exclusions
                .exclusion(&repo_path)
                .map(ManifestDisposition::Excluded)
                .unwrap_or(ManifestDisposition::Included);
            entries.push(RepositoryEntry::new(
                repo_path,
                bytes.clone(),
                atomic_core::output::DEFAULT_REGULAR_MODE,
                InodeKind::Regular,
                None,
                disposition,
            )?);
        }
        // The order-invariant identity of this exact closure: additive SetId
        // over every closure member in dependency-first order.
        let mut set_id = SetId::ZERO;
        for change_id in closure.iter_dependency_first().copied() {
            let hash = txn
                .get_external(change_id)
                .map_err(repo_error)?
                .ok_or_else(|| {
                    ProjectTreeError::Repository(format!(
                        "closure change {} has no external hash",
                        change_id.get()
                    ))
                })?;
            set_id = set_id.add(&hash);
        }
        let manifest = RepositoryManifest::new(set_id, policy.root().content_key, entries)?;
        ProjectTree::from_manifest(manifest, policy)
    }
}

pub(super) fn git_oid_hex(oid: &GitObjectId) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = Vec::with_capacity(oid.as_bytes().len() * 2);
    for byte in oid.as_bytes() {
        output.push(HEX[usize::from(byte >> 4)]);
        output.push(HEX[usize::from(byte & 0x0f)]);
    }
    output
}

fn parse_gitlink_bytes(
    algorithm: GitHashAlgorithm,
    bytes: &[u8],
) -> Result<GitObjectId, ProjectTreeError> {
    let expected = match algorithm {
        GitHashAlgorithm::Sha1 => 40,
        GitHashAlgorithm::Sha256 => 64,
    };
    if bytes.len() != expected || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(ProjectTreeError::InvalidGitlink(format!(
            "expected {expected} hexadecimal bytes"
        )));
    }
    let mut decoded = Vec::with_capacity(expected / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let text = std::str::from_utf8(pair).expect("ASCII hex checked");
        decoded.push(u8::from_str_radix(text, 16).expect("hex checked"));
    }
    GitObjectId::new(algorithm, decoded)
        .map_err(|error| ProjectTreeError::GitObjectId(error.to_string()))
}

fn repo_error(error: impl fmt::Display) -> ProjectTreeError {
    ProjectTreeError::Repository(error.to_string())
}

impl From<RepositoryError> for ProjectTreeError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(error.to_string())
    }
}

#[derive(Debug, Error)]
pub enum ProjectTreeError {
    #[error("invalid repository path: {0}")]
    InvalidPath(String),
    #[error("lossless raw-byte native paths are unsupported on this platform")]
    UnsupportedPlatformPath,
    #[error("unsupported {model} version {found}; expected {expected}")]
    UnsupportedVersion {
        model: &'static str,
        found: u8,
        expected: u8,
    },
    #[error("duplicate repository path {0:?}")]
    DuplicatePath(RepoPath),
    #[error("path collides with a file or directory: {0:?}")]
    PathCollision(RepoPath),
    #[error("invalid inode mode {0:#o}")]
    InvalidMode(u16),
    #[error("invalid gitlink: {0}")]
    InvalidGitlink(String),
    #[error("Git object ID error: {0}")]
    GitObjectId(String),
    #[error("Git object algorithm does not match the tree algorithm")]
    ObjectAlgorithmMismatch,
    #[error("missing Git object {0:?}")]
    MissingGitObject(GitObjectId),
    #[error("malformed Git tree: {0}")]
    MalformedGitTree(String),
    #[error("unsupported Git tree mode {0:#o}")]
    UnsupportedGitMode(u32),
    #[error("manifest conversion-policy root does not match the supplied policy")]
    PolicyRootMismatch,
    #[error("prospective import tree mismatch: expected {expected}, got {actual}")]
    ProspectiveTreeMismatch { expected: String, actual: String },
    #[error("repository projection failed: {0}")]
    Repository(String),
    #[error("truncated canonical encoding")]
    TruncatedEncoding,
    #[error("canonical encoding has trailing bytes")]
    TrailingEncoding,
}

fn algorithm_tag(algorithm: GitHashAlgorithm) -> u8 {
    match algorithm {
        GitHashAlgorithm::Sha1 => 1,
        GitHashAlgorithm::Sha256 => 2,
    }
}

struct Encoder(Vec<u8>);
impl Encoder {
    fn new() -> Self {
        Self(Vec::new())
    }
    fn finish(self) -> Vec<u8> {
        self.0
    }
    fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn tag(&mut self, bytes: &[u8]) {
        self.bytes(bytes);
    }
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.raw(&value.to_be_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.u32(bytes.len() as u32);
        self.raw(bytes);
    }
    fn optional_bytes(&mut self, bytes: Option<&[u8]>) {
        match bytes {
            Some(bytes) => {
                self.u8(1);
                self.bytes(bytes);
            }
            None => self.u8(0),
        }
    }
    fn git_oid(&mut self, oid: Option<&GitObjectId>) {
        match oid {
            Some(oid) => {
                self.u8(algorithm_tag(oid.algorithm()));
                self.bytes(oid.as_bytes());
            }
            None => self.u8(0),
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}
impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }
    fn version(&mut self, expected: u8, model: &'static str) -> Result<(), ProjectTreeError> {
        let found = self.u8()?;
        if found == expected {
            Ok(())
        } else {
            Err(ProjectTreeError::UnsupportedVersion {
                model,
                found,
                expected,
            })
        }
    }
    fn u8(&mut self) -> Result<u8, ProjectTreeError> {
        let value = *self
            .bytes
            .get(self.cursor)
            .ok_or(ProjectTreeError::TruncatedEncoding)?;
        self.cursor += 1;
        Ok(value)
    }
    fn bytes(&mut self) -> Result<&'a [u8], ProjectTreeError> {
        let length_bytes = self
            .bytes
            .get(self.cursor..self.cursor + 4)
            .ok_or(ProjectTreeError::TruncatedEncoding)?;
        self.cursor += 4;
        let length = u32::from_be_bytes(length_bytes.try_into().expect("four bytes")) as usize;
        let value = self
            .bytes
            .get(self.cursor..self.cursor + length)
            .ok_or(ProjectTreeError::TruncatedEncoding)?;
        self.cursor += length;
        Ok(value)
    }
    fn finish(self) -> Result<(), ProjectTreeError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(ProjectTreeError::TrailingEncoding)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(algorithm: GitHashAlgorithm, byte: u8) -> GitObjectId {
        let length = match algorithm {
            GitHashAlgorithm::Sha1 => 20,
            GitHashAlgorithm::Sha256 => 32,
        };
        GitObjectId::new(algorithm, vec![byte; length]).unwrap()
    }

    #[test]
    fn repo_path_is_raw_versioned_and_canonical() {
        let path = RepoPath::from_bytes(b"src/\xffname").unwrap();
        assert_eq!(
            RepoPath::from_versioned_bytes(&path.to_versioned_bytes()).unwrap(),
            path
        );
        assert!(RepoPath::from_bytes(b"/absolute").is_err());
        assert!(RepoPath::from_bytes(b"a//b").is_err());
        assert!(RepoPath::from_bytes(b"a/../b").is_err());
        assert!(RepoPath::from_bytes(b"a\0b").is_err());
        assert_eq!(path.escaped(), "src/%FFname");
    }

    #[test]
    fn model_roots_are_deterministic_and_sensitive() {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        assert_eq!(policy.root(), policy.clone().root());
        let index = GitIndexState::new(GitHashAlgorithm::Sha1, vec![]);
        assert_eq!(index.root(), index.clone().root());
        let observation = WorktreeObservation {
            version: WORKTREE_OBSERVATION_VERSION,
            platform: policy.platform.clone(),
            entries: vec![],
        };
        assert_eq!(observation.root(), observation.clone().root());
        let mut changed = policy.clone();
        changed.object_format = GitHashAlgorithm::Sha256;
        assert_ne!(policy.root(), changed.root());
    }

    #[test]
    fn atomic_and_git_builders_agree_for_both_algorithms() {
        for algorithm in [GitHashAlgorithm::Sha1, GitHashAlgorithm::Sha256] {
            let policy = ConversionPolicy::new(algorithm);
            let entries = vec![
                RepositoryEntry::new(
                    RepoPath::from_bytes(b"empty").unwrap(),
                    vec![],
                    0o644,
                    InodeKind::Regular,
                    None,
                    ManifestDisposition::Included,
                )
                .unwrap(),
                RepositoryEntry::new(
                    RepoPath::from_bytes(b"bin/run").unwrap(),
                    b"#!/bin/sh\n".to_vec(),
                    0o755,
                    InodeKind::Regular,
                    None,
                    ManifestDisposition::Included,
                )
                .unwrap(),
                RepositoryEntry::new(
                    RepoPath::from_bytes(b"links/current").unwrap(),
                    b"../empty".to_vec(),
                    0o777,
                    InodeKind::Symlink,
                    None,
                    ManifestDisposition::Included,
                )
                .unwrap(),
                RepositoryEntry::new(
                    RepoPath::from_bytes(b"vendor/sub").unwrap(),
                    git_oid_hex(&oid(algorithm, 7)),
                    0o644,
                    InodeKind::Gitlink,
                    Some(oid(algorithm, 7)),
                    ManifestDisposition::Included,
                )
                .unwrap(),
                RepositoryEntry::new(
                    RepoPath::from_bytes(b".atomic/private").unwrap(),
                    b"secret".to_vec(),
                    0o644,
                    InodeKind::Regular,
                    None,
                    ManifestDisposition::Excluded(ExclusionReason::AtomicPrivate),
                )
                .unwrap(),
                RepositoryEntry::new(
                    RepoPath::from_bytes(b"nested/data").unwrap(),
                    vec![0, 255, 10],
                    0o644,
                    InodeKind::Regular,
                    None,
                    ManifestDisposition::Included,
                )
                .unwrap(),
            ];
            let manifest = RepositoryManifest::new(
                SetId::from_bytes([9; 32]),
                policy.root().content_key.clone(),
                entries,
            )
            .unwrap();
            let projected = ProjectTree::from_manifest(manifest.clone(), &policy).unwrap();
            let parsed = projected
                .git
                .objects
                .parse_manifest(&projected.git.root, &policy)
                .unwrap();
            let expected: Vec<_> = manifest
                .included_entries()
                .map(|entry| {
                    (
                        &entry.path,
                        &entry.repository_bytes,
                        entry.git_mode(),
                        entry.kind,
                        entry.gitlink.as_ref(),
                    )
                })
                .collect();
            let actual: Vec<_> = parsed
                .included_entries()
                .map(|entry| {
                    (
                        &entry.path,
                        &entry.repository_bytes,
                        entry.git_mode(),
                        entry.kind,
                        entry.gitlink.as_ref(),
                    )
                })
                .collect();
            assert_eq!(actual, expected);
            assert!(!parsed
                .entries
                .iter()
                .any(|entry| entry.path.as_bytes().starts_with(b".atomic/")));
            assert_eq!(
                git_object_id(
                    algorithm,
                    GitObjectKind::Tree,
                    projected
                        .git
                        .objects
                        .get(&projected.git.root)
                        .unwrap()
                        .bytes
                        .as_slice()
                )
                .unwrap(),
                projected.git.root
            );
        }
    }

    #[test]
    fn git_tree_parser_rejects_algorithm_mismatch() {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let manifest =
            RepositoryManifest::new(SetId::ZERO, policy.root().content_key.clone(), vec![])
                .unwrap();
        let projected = ProjectTree::from_manifest(manifest, &policy).unwrap();
        let other = ConversionPolicy::new(GitHashAlgorithm::Sha256);
        assert!(matches!(
            projected
                .git
                .objects
                .parse_manifest(&projected.git.root, &other),
            Err(ProjectTreeError::ObjectAlgorithmMismatch)
        ));
    }
}
