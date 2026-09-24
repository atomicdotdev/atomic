//! Versioned local checkpoint persistence for the colocated Git bridge.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use atomic_repository::Repository;

use super::observation::{observe_git, GitObservation, HeadObservation, ObservationError};

pub(crate) const CHECKPOINT_VERSION: u32 = 2;
const ATOMIC_MANIFEST_DOMAIN: &[u8] = b"atomic:bridge:atomic-visible-regular-files:v1\0";
const CHECKPOINT_RELATIVE_PATH: &str = ".atomic/bridge/workspace.json";

/// A manifest root carried as evidence rather than as a claim of full RFC
/// repository-manifest equivalence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ManifestRootEvidence {
    pub kind: ManifestRootKind,
    pub value: String,
    /// `true` means the root covers only the explicitly named subset.
    pub provisional: bool,
}

impl ManifestRootEvidence {
    pub(crate) fn git_tree(value: impl Into<String>) -> Self {
        Self {
            kind: ManifestRootKind::GitTreeOid,
            value: value.into(),
            provisional: false,
        }
    }

    fn atomic_visible_regular_files(value: impl Into<String>) -> Self {
        Self {
            kind: ManifestRootKind::AtomicVisibleRegularFilesBlake3V1,
            value: value.into(),
            provisional: true,
        }
    }
}

impl std::fmt::Display for ManifestRootEvidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}:{}{}",
            self.kind,
            self.value,
            if self.provisional {
                " (provisional)"
            } else {
                ""
            }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ManifestRootKind {
    /// Exact Git tree object identity.
    GitTreeOid,
    /// Domain-separated digest over Atomic's UTF-8 visible regular-file paths
    /// and graph-derived bytes. Modes, kinds, filters, and raw non-UTF-8 paths
    /// are not covered.
    AtomicVisibleRegularFilesBlake3V1,
}

impl std::fmt::Display for ManifestRootKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::GitTreeOid => "git-tree-oid",
            Self::AtomicVisibleRegularFilesBlake3V1 => "atomic-visible-regular-files-blake3-v1",
        })
    }
}

/// Git administrative identity resolved by the read-only observer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct GitAdminIdentity {
    pub worktree_root: Option<PathBuf>,
    pub worktree_git_dir: PathBuf,
    pub common_dir: PathBuf,
    pub index_path: PathBuf,
}

/// Atomic side of one observed bridge checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CheckpointAtomicState {
    pub view: String,
    pub state: String,
    pub manifest_root: Option<ManifestRootEvidence>,
}

/// Version-2 bridge checkpoint.
///
/// Fields remain flat so the experimental bridge can upgrade the version-1
/// JSON without changing its existing direction-classification call sites.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct BridgeCheckpoint {
    pub version: u32,
    pub view: String,
    pub atomic_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atomic_manifest_root: Option<ManifestRootEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_head_symref: Option<String>,
    pub git_head: String,
    pub git_tree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_manifest_root: Option<ManifestRootEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_index_tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_index_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_refs_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_admin: Option<GitAdminIdentity>,
}

impl BridgeCheckpoint {
    pub(crate) fn legacy_compatible(
        view: impl Into<String>,
        atomic_state: impl Into<String>,
        git_head: impl Into<String>,
        git_tree: impl Into<String>,
    ) -> Self {
        let view = view.into();
        let git_tree = git_tree.into();
        Self {
            version: CHECKPOINT_VERSION,
            git_head_symref: Some(format!("refs/heads/{view}")),
            view,
            atomic_state: atomic_state.into(),
            atomic_manifest_root: None,
            git_head: git_head.into(),
            git_manifest_root: Some(ManifestRootEvidence::git_tree(git_tree.clone())),
            git_index_tree: Some(git_tree.clone()),
            git_tree,
            git_index_digest: None,
            git_refs_digest: None,
            git_admin: None,
        }
    }
}

/// State already verified by the bridge before a checkpoint is published.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VerifiedCheckpointInput<'a> {
    pub view: &'a str,
    pub atomic_state: &'a str,
    pub git_head: &'a str,
    pub git_tree: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckpointError {
    #[error("cannot read bridge checkpoint '{path}': {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("bridge checkpoint '{path}' is malformed: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("unsupported bridge checkpoint version {version}")]
    UnsupportedVersion { version: u32 },

    #[error("cannot write bridge checkpoint: {0}")]
    Write(#[source] std::io::Error),

    #[error("cannot encode bridge checkpoint: {0}")]
    Encode(#[source] serde_json::Error),

    #[error("cannot inspect Atomic checkpoint state: {0}")]
    Atomic(String),

    #[error(transparent)]
    Observation(#[from] ObservationError),

    #[error("a bridge checkpoint requires a Git working tree")]
    NoGit,

    #[error("a bridge checkpoint requires an attached Git HEAD")]
    UnsupportedHead,

    #[error(
        "checkpoint input changed while observing {field}: expected {expected}, found {current}"
    )]
    ObservedStateChanged {
        field: &'static str,
        expected: String,
        current: String,
    },
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

#[derive(Deserialize)]
struct BridgeCheckpointV1 {
    version: u32,
    view: String,
    atomic_state: String,
    git_head: String,
    git_tree: String,
}

pub(crate) fn checkpoint_path(root: &Path) -> PathBuf {
    root.join(CHECKPOINT_RELATIVE_PATH)
}

/// Read and upgrade a local checkpoint without rewriting it.
pub(crate) fn read_checkpoint(root: &Path) -> Result<Option<BridgeCheckpoint>, CheckpointError> {
    let path = checkpoint_path(root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CheckpointError::Read { path, source }),
    };
    decode_checkpoint_at(&bytes, path).map(Some)
}

fn decode_checkpoint_at(bytes: &[u8], path: PathBuf) -> Result<BridgeCheckpoint, CheckpointError> {
    let probe: VersionProbe =
        serde_json::from_slice(bytes).map_err(|source| CheckpointError::Malformed {
            path: path.clone(),
            source,
        })?;

    match probe.version {
        1 => {
            let legacy: BridgeCheckpointV1 =
                serde_json::from_slice(bytes).map_err(|source| CheckpointError::Malformed {
                    path: path.clone(),
                    source,
                })?;
            debug_assert_eq!(legacy.version, 1);
            Ok(BridgeCheckpoint::legacy_compatible(
                legacy.view,
                legacy.atomic_state,
                legacy.git_head,
                legacy.git_tree,
            ))
        }
        CHECKPOINT_VERSION => serde_json::from_slice(bytes)
            .map_err(|source| CheckpointError::Malformed { path, source }),
        version => Err(CheckpointError::UnsupportedVersion { version }),
    }
}

/// Atomically write a version-2 checkpoint.
pub(crate) fn write_checkpoint(
    root: &Path,
    checkpoint: &BridgeCheckpoint,
) -> Result<(), CheckpointError> {
    let directory = root.join(".atomic/bridge");
    fs::create_dir_all(&directory).map_err(CheckpointError::Write)?;
    let destination = checkpoint_path(root);
    let temporary = directory.join(format!(".workspace.json.{}.tmp", std::process::id()));

    let mut persisted = checkpoint.clone();
    persisted.version = CHECKPOINT_VERSION;
    let bytes = serde_json::to_vec_pretty(&persisted).map_err(CheckpointError::Encode)?;

    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(CheckpointError::Write(error));
    }
    Ok(())
}

/// Observe the current Atomic view using graph-derived bytes only.
///
/// This does not call `Repository::status` and never reads working-copy files.
pub(crate) fn observe_current_atomic(
    root: &Path,
) -> Result<CheckpointAtomicState, CheckpointError> {
    let repo = Repository::open_readonly(root).map_err(|error| {
        CheckpointError::Atomic(format!("cannot open repository read-only: {error}"))
    })?;
    let working_copy = repo
        .require_working_copy_id()
        .map_err(|error| CheckpointError::Atomic(error.to_string()))?;
    let view = repo
        .desired_view_name(working_copy)
        .map_err(|error| CheckpointError::Atomic(error.to_string()))?;
    observe_atomic_on_handle(&repo, &view)
}

fn observe_atomic_view(root: &Path, view: &str) -> Result<CheckpointAtomicState, CheckpointError> {
    let repo = Repository::open_readonly(root).map_err(|error| {
        CheckpointError::Atomic(format!("cannot open repository read-only: {error}"))
    })?;
    observe_atomic_on_handle(&repo, view)
}

fn observe_atomic_on_handle(
    repo: &Repository,
    view: &str,
) -> Result<CheckpointAtomicState, CheckpointError> {
    let state = repo
        .get_view_info(view)
        .map_err(|error| CheckpointError::Atomic(error.to_string()))?
        .state
        .to_string();
    let mut paths: Vec<_> = repo
        .visible_file_paths(view)
        .map_err(|error| CheckpointError::Atomic(error.to_string()))?
        .into_iter()
        .collect();
    paths.sort();

    // RFC §8.3 (CB-8B): a conflicted view's materialized state IS the
    // marker projection. The checkpoint's atomic-visible manifest must
    // describe exactly the bytes on disk and in the conflict snapshot's Git
    // tree, so the workspace classifies as aligned instead of inventing
    // "pending edits" for the lossy marker representation.
    let conflict_markers = repo
        .capture_view_conflict_set_with_markers(view)
        .map_err(|error| CheckpointError::Atomic(error.to_string()))?
        .map(|(object, markers)| (object.hash().ok(), markers));

    let mut hasher = blake3::Hasher::new();
    hasher.update(ATOMIC_MANIFEST_DOMAIN);
    for path in paths {
        let content = match &conflict_markers {
            Some((_, markers)) if markers.contains_key(&path) => {
                markers.get(&path).cloned().expect("marker bytes")
            }
            _ => repo
                .get_file_content_on_view(&path, view)
                .map_err(|error| CheckpointError::Atomic(error.to_string()))?
                .ok_or_else(|| {
                    CheckpointError::Atomic(format!(
                        "visible path '{path}' has no graph-derived content on view '{view}'"
                    ))
                })?,
        };
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(&(content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }

    Ok(CheckpointAtomicState {
        view: view.to_string(),
        state,
        manifest_root: Some(ManifestRootEvidence::atomic_visible_regular_files(
            hasher.finalize().to_hex().to_string(),
        )),
    })
}

/// Re-observe the verified bridge state and publish it as a version-2 checkpoint.
pub(crate) fn write_verified_checkpoint(
    root: &Path,
    input: VerifiedCheckpointInput<'_>,
) -> Result<BridgeCheckpoint, CheckpointError> {
    let atomic = observe_atomic_view(root, input.view)?;
    if atomic.state.as_str() != input.atomic_state {
        return Err(CheckpointError::ObservedStateChanged {
            field: "Atomic state",
            expected: input.atomic_state.to_string(),
            current: atomic.state,
        });
    }

    let observation = observe_git(root)?;
    let GitObservation::Repository(repository) = observation else {
        return Err(CheckpointError::NoGit);
    };
    // CB-8A: Draft views stay detached at their projection commit (RFC
    // §8.1); the checkpoint records a `None` symref for detached states so
    // the workspace entry protocol keeps them aligned, exactly like the
    // CB-7A anchor does for adopted ephemeral views.
    let (symref, oid) = match &repository.head {
        HeadObservation::Attached { symref, oid } => (Some(symref.clone()), *oid),
        HeadObservation::Detached { oid } => (None, *oid),
        _ => return Err(CheckpointError::UnsupportedHead),
    };
    let head_tree = repository
        .head_tree_oid
        .map(|oid| oid.to_string())
        .ok_or(CheckpointError::UnsupportedHead)?;
    if oid.to_string().as_str() != input.git_head {
        return Err(CheckpointError::ObservedStateChanged {
            field: "Git HEAD",
            expected: input.git_head.to_string(),
            current: oid.to_string(),
        });
    }
    if head_tree.as_str() != input.git_tree {
        return Err(CheckpointError::ObservedStateChanged {
            field: "Git HEAD tree",
            expected: input.git_tree.to_string(),
            current: head_tree,
        });
    }

    let checkpoint = BridgeCheckpoint {
        version: CHECKPOINT_VERSION,
        view: atomic.view,
        atomic_state: input.atomic_state.to_string(),
        atomic_manifest_root: atomic.manifest_root,
        git_head_symref: symref,
        git_head: input.git_head.to_string(),
        git_tree: input.git_tree.to_string(),
        git_manifest_root: Some(ManifestRootEvidence::git_tree(input.git_tree)),
        git_index_tree: repository.index.tree_oid.map(|oid| oid.to_string()),
        git_index_digest: Some(repository.index.canonical_digest.0.clone()),
        git_refs_digest: Some(repository.refs_digest.0.clone()),
        git_admin: Some(GitAdminIdentity {
            worktree_root: repository.paths.worktree_root.clone(),
            worktree_git_dir: repository.paths.worktree_git_dir.clone(),
            common_dir: repository.paths.common_dir.clone(),
            index_path: repository.paths.index_path.clone(),
        }),
    };
    write_checkpoint(root, &checkpoint)?;
    Ok(checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_checkpoint_decodes_and_upgrades_without_rewrite() {
        let bytes = br#"{
            "version": 1,
            "view": "main",
            "atomic_state": "atomic-1",
            "git_head": "1111111111111111111111111111111111111111",
            "git_tree": "2222222222222222222222222222222222222222"
        }"#;
        let checkpoint =
            decode_checkpoint_at(bytes, PathBuf::from("workspace.json")).expect("decode v1");

        assert_eq!(checkpoint.version, CHECKPOINT_VERSION);
        assert_eq!(
            checkpoint.git_head_symref.as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(
            checkpoint.git_index_tree.as_deref(),
            Some(checkpoint.git_tree.as_str())
        );
        assert!(checkpoint.git_index_digest.is_none());
        assert!(checkpoint.git_refs_digest.is_none());
        assert!(checkpoint.git_admin.is_none());
        assert_eq!(
            checkpoint.git_manifest_root,
            Some(ManifestRootEvidence::git_tree(checkpoint.git_tree.clone()))
        );
        assert!(checkpoint.atomic_manifest_root.is_none());
    }

    #[test]
    fn v2_checkpoint_roundtrips_all_evidence() {
        let root = tempfile::tempdir().expect("tempdir");
        let checkpoint = BridgeCheckpoint {
            version: CHECKPOINT_VERSION,
            view: "agent/session-1".to_string(),
            atomic_state: "atomic-2".to_string(),
            atomic_manifest_root: Some(ManifestRootEvidence::atomic_visible_regular_files(
                "atomic-root",
            )),
            git_head_symref: Some("refs/heads/main".to_string()),
            git_head: "1111111111111111111111111111111111111111".to_string(),
            git_tree: "2222222222222222222222222222222222222222".to_string(),
            git_manifest_root: Some(ManifestRootEvidence::git_tree(
                "2222222222222222222222222222222222222222",
            )),
            git_index_tree: Some("2222222222222222222222222222222222222222".to_string()),
            git_index_digest: Some("index-digest".to_string()),
            git_refs_digest: Some("refs-digest".to_string()),
            git_admin: Some(GitAdminIdentity {
                worktree_root: Some(root.path().to_path_buf()),
                worktree_git_dir: root.path().join(".git/worktrees/current"),
                common_dir: root.path().join(".git"),
                index_path: root.path().join(".git/worktrees/current/index"),
            }),
        };

        write_checkpoint(root.path(), &checkpoint).expect("write v2");
        let decoded = read_checkpoint(root.path())
            .expect("read v2")
            .expect("checkpoint present");
        assert_eq!(decoded, checkpoint);
        assert!(decoded
            .atomic_manifest_root
            .as_ref()
            .is_some_and(|root| root.provisional));
        assert!(decoded
            .git_manifest_root
            .as_ref()
            .is_some_and(|root| !root.provisional));
    }
}
