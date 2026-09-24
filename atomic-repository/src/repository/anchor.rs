//! CB-7A: bridge anchoring and bound Git HEAD adoption.
//!
//! Normative source: RFC-ATOMIC-GIT-CAUSAL-BRIDGE §5.1–5.2, §7.1–7.5, §8.1,
//! §12.1–12.7 and tracker unit CB-7A.
//!
//! # Anchoring (RFC Phase 7 task 1, §7.2 exit step 6)
//!
//! `bridge enable` proves complete repository/index/worktree equivalence under
//! one conversion policy and only then creates a signed, immutable Git-state
//! binding (the Anchor) plus a verified working-copy checkpoint. Mismatches
//! are typed refusals that offer the explicit `--adopt-atomic` remediation;
//! unbound adoption and `--adopt-git` stay unavailable until Phase 9 foreign
//! synthesis (CB-9B/9C).
//!
//! # Bound HEAD adoption (§7.3)
//!
//! A changed or detached Git HEAD with a valid binding is resurrected and
//! mapped *before* ordinary filesystem scanning (§12.2). When Git reports a
//! stage-0 index and a worktree clean relative to the new HEAD, the target
//! state is adopted with zero tracked-file writes and the verified checkpoint
//! is rewritten only after HEAD/index re-observation. Everything else returns
//! typed remediation; unexplained differences are never claimed clean (§12.7).
//!
//! # Identity selection (§7.5, §8.1)
//!
//! Bindings bind commit state; a binding's `view_hint` is a hint, never
//! authority (§5.1). Adoption resolves identity in this documented order:
//!
//! 1. **detached HEAD** → an ephemeral Draft view `git/<short-oid>` (parent:
//!    the hinted view when it exists and is Shared, else none), reusing the
//!    view the workspace head map already records for that commit;
//! 2. **attached HEAD whose branch was just created** (single `branch:
//!    Created from …` reflog entry) while the commit maps to an ephemeral
//!    view → `git switch -c` semantics: that ephemeral view is renamed in one
//!    atomic transaction that preserves its change log, Merkle chain, tags,
//!    conflicts, and identity (RFC §7.5);
//! 3. **attached HEAD with a binding hint naming an existing view** → adopt
//!    into that view — the binding is the mapping authority, and a same-name
//!    view is never guessed;
//! 4. **attached HEAD with a binding hint naming a missing view** → create
//!    the hinted Shared view and adopt into it;
//! 5. otherwise → an ephemeral `git/<short-oid>` Draft view with the nearest
//!    hinted Shared parent or none.
//!
//! Short-OID collisions grow the prefix one character at a time while a
//! *different* commit owns the same view name; exhausting the full OID is a
//! typed refusal. General branch/view/remote reconciliation (branch renames
//! while attached, shared-view mapping, remote refs) stays with CB-10A and
//! returns explicit typed refusals here.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use atomic_core::operation::{
    ActorRef, EffectPlan, EffectTarget, EffectValue, GitHashAlgorithm, GitObjectId, OperationKind,
    RepoStateRef, ViewStateRef,
};
use atomic_core::pristine::{
    GraphTxnT, MutTxnT, TagMutTxnT, TagTxnT, ViewScope, ViewTxnT, WorkingCopyMutTxnT,
    WorkingCopyTxnT,
};
use atomic_core::types::{Base32, Merkle, WorkingCopyId};

use git2::Repository as GitRepository;

use crate::git_binding::{
    BindingSigner, CausalOrigin, GitObjectFormat, GitOid, GitStateBinding, GitStateBindingPayload,
    BINDING_VERSION,
};
use crate::repository::resurrection::ExactResurrection;
use crate::RepositoryError;

use super::equivalence::{compare_project_state, EquivalenceClaims, EquivalenceLayer};
use super::git_observation::{GitHeadObservation, ObservationError};
use super::locks::WorkingCopyOperationLockGuard;
use super::operation::{current_operation_timestamp_ms, working_copy_state_ref};
use super::project_tree::{ConversionPolicy, PlatformCapabilities};
use super::workspace_txn::{
    read_workspace_checkpoint, write_workspace_checkpoint, WorkspaceCheckpoint,
};
use super::{observe_git_index, Repository};

/// The workspace-local commit → view map, under the bridge-private directory.
pub const HEAD_MAP_RELATIVE: &str = ".atomic/bridge/head-map.json";

/// The shortest Git short-OID used in ephemeral view names (RFC §7.5).
const EPHEMERAL_MIN_PREFIX: usize = 7;

// ── Anchoring refusals (AC-1) ────────────────────────────────────────────

/// Typed anchor refusals. Nothing is mutated before a refusal is produced and
/// every variant names its remediation (RFC §7, Phase 7 task 1).
#[derive(Debug, thiserror::Error)]
pub enum BridgeAnchorRefusal {
    #[error(
        "cannot anchor an unborn Git HEAD ('{symref}' has no commit); create at least one \
         commit before enabling the bridge"
    )]
    UnbornHead { symref: String },
    #[error(
        "cannot anchor Git HEAD pointing at missing target '{symref}'; restore the branch in \
         Git before enabling the bridge"
    )]
    MissingHeadTarget { symref: String },
    #[error(
        "cannot anchor a detached Git HEAD at {oid}; checkout a branch or bind this commit \
         explicitly with 'atomic git bridge binding publish --key-file …'"
    )]
    DetachedHead { oid: String },
    #[error(
        "cannot anchor: {detail}; pending work is preserved as-is and is not re-globalized \
         until CB-7B — commit or record it first, then retry"
    )]
    DirtyState { detail: String },
    #[error(
        "bridge anchor refused: Git and Atomic states diverge; run \
         'atomic git bridge enable --adopt-atomic' to project the Atomic state onto Git, or \
         reconcile manually — Atomic never silently merges the two. Mismatches: {report}"
    )]
    Mismatch { report: String },
    #[error(
        "--adopt-git is unavailable: unbound Git adoption ships with Phase 9 foreign \
         synthesis (CB-9B/9C); bind this commit first with \
         'atomic git bridge binding publish --key-file …'"
    )]
    AdoptGitUnavailable,
}

/// Anchor failures that are not refusals: infrastructure or verification.
#[derive(Debug, thiserror::Error)]
pub enum BridgeAnchorError {
    #[error(transparent)]
    Refusal(#[from] BridgeAnchorRefusal),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Observation(#[from] ObservationError),
    #[error("git error: {0}")]
    Git(String),
}

/// Successful anchor: the signed immutable binding plus the verified checkpoint.
#[derive(Debug)]
pub struct BridgeAnchorOutcome {
    /// The signed Anchor binding (immutable, create-only publication).
    pub binding: GitStateBinding,
    /// Whether the binding ref was created (fresh) or replayed an identical
    /// existing fact (idempotent repeat enable).
    pub publication: crate::BindingPublication,
    /// The Atomic view the Anchor binds.
    pub view: String,
    /// The bound view state.
    pub state: Merkle,
    /// The bound Git commit, lowercase hex.
    pub git_head: String,
    /// The bound Git tree, lowercase hex.
    pub git_tree: String,
}

// ── Bound HEAD adoption (AC-2 / AC-3) ────────────────────────────────────

/// CB-7A §7.3 outcome for one bound HEAD reconciliation attempt.
#[derive(Debug)]
pub enum AdoptBoundHead {
    /// HEAD/index already match the workspace checkpoint; nothing was done.
    Aligned,
    /// A verified binding was resurrected and mapped; no tracked file was
    /// rewritten (RFC §7.3 "adopt: no file writes").
    Adopted {
        /// The view the workspace now points at.
        view: String,
        /// The exact Merkle state of the adopted view (the binding's state).
        state: Merkle,
        /// Whether the adopted view is an ephemeral `git/<oid>` Draft view.
        ephemeral: bool,
        /// The journaled `ImportGitHead` operation, when this call advanced
        /// the working copy. Idempotent retries return `None`.
        import: Option<atomic_core::types::OperationId>,
        /// The journaled `ResurrectBinding` operation, when the closure was
        /// newly applied. Idempotent re-runs return `None`.
        resurrect: Option<atomic_core::types::OperationId>,
    },
}

/// The head → view mapping decision for one adoption.
#[allow(dead_code)] // parent is parsed for future multi-level policy
struct HeadTarget {
    view: String,
    ephemeral: bool,
    rename_from: Option<String>,
    parent: Option<String>,
}

/// The rename fast-path result: the view the workspace now points at.
struct RenamedAdoption {
    view: String,
    state: Merkle,
}

// ── Head map (private workspace evidence, RFC §7.5) ──────────────────────

/// The workspace-local commit → view mapping entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeadMapEntry {
    /// The view this workspace mapped the commit to.
    pub view: String,
    /// Whether the view was created as an ephemeral `git/<oid>` Draft view.
    pub ephemeral: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct HeadMapWire {
    version: u32,
    #[serde(default)]
    mappings: BTreeMap<String, HeadMapEntry>,
}

/// Read the workspace-local head map. It lives under the bridge-private
/// directory, is never projected into Git, and a missing file reads as an
/// empty map; malformed or future-versioned maps fail closed.
pub fn read_head_map(root: &Path) -> Result<BTreeMap<String, HeadMapEntry>, RepositoryError> {
    let path = root.join(HEAD_MAP_RELATIVE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(RepositoryError::InvalidRepository {
                reason: format!("cannot read bridge head map '{}': {error}", path.display()),
            })
        }
    };
    let wire: HeadMapWire =
        serde_json::from_slice(&bytes).map_err(|error| RepositoryError::InvalidRepository {
            reason: format!("bridge head map '{}' is malformed: {error}", path.display()),
        })?;
    if wire.version != 1 {
        return Err(RepositoryError::InvalidRepository {
            reason: format!("unsupported bridge head map version {}", wire.version),
        });
    }
    Ok(wire.mappings)
}

/// The §7.5 view a detached commit imports into (CB-7B reconcile routing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetachedImportTarget {
    /// The workspace already maps this commit's baseline: head map entry, or
    /// the verified checkpoint's view when the checkpoint HEAD is a
    /// first-parent ancestor of the detached commit (§7.3 "synthesize into the
    /// mapped view, base = last bound workspace state").
    Mapped { view: String },
    /// No workspace mapping covers this commit: a fresh ephemeral Draft
    /// `git/<short-oid>` view (§7.5), which the caller creates.
    Ephemeral { view: String },
}

impl Repository {
    /// Resolve the §7.5 target view for a detached commit no branch carries.
    ///
    /// Order mirrors the adoption identity rules: the workspace head map first
    /// (the adoption itself records mappings), then the verified checkpoint
    /// when its HEAD is a first-parent ancestor of the observed commit, and
    /// only then a fresh ephemeral name. Read-only; creating the view is the
    /// caller's journaled decision.
    pub fn detached_import_target(
        &self,
        head_hex: &str,
    ) -> Result<DetachedImportTarget, RepositoryError> {
        let head = head_hex.to_ascii_lowercase();
        let mappings = read_head_map(self.root())?;
        if let Some(entry) = mappings.get(&head) {
            if self.view_exists(&entry.view)? {
                return Ok(DetachedImportTarget::Mapped {
                    view: entry.view.clone(),
                });
            }
        }
        if let Some(checkpoint) = read_workspace_checkpoint(self.root())? {
            let checkpoint_head = checkpoint.git_head.to_ascii_lowercase();
            if checkpoint_head == head && self.view_exists(&checkpoint.view)? {
                return Ok(DetachedImportTarget::Mapped {
                    view: checkpoint.view.clone(),
                });
            }
            if self.view_exists(&checkpoint.view)?
                && self.is_first_parent_ancestor(&checkpoint_head, &head)?
            {
                return Ok(DetachedImportTarget::Mapped {
                    view: checkpoint.view.clone(),
                });
            }
        }
        Ok(DetachedImportTarget::Ephemeral {
            view: self.ephemeral_view_name(&head)?,
        })
    }

    /// Whether `ancestor_hex` is a first-parent ancestor of `descendant_hex`.
    ///
    /// Bounded walk (10,000 first-parent steps) so corrupt or absurd history
    /// fails closed instead of scanning unbounded history.
    fn is_first_parent_ancestor(
        &self,
        ancestor_hex: &str,
        descendant_hex: &str,
    ) -> Result<bool, RepositoryError> {
        let git = git2::Repository::discover(self.root()).map_err(|error| {
            RepositoryError::InvalidRepository {
                reason: format!("cannot open the Git repository for ancestry: {error}"),
            }
        })?;
        let mut oid = git2::Oid::from_str(descendant_hex).map_err(|error| {
            RepositoryError::InvalidRepository {
                reason: format!("commit '{descendant_hex}' is not a valid object ID: {error}"),
            }
        })?;
        let ancestor = git2::Oid::from_str(ancestor_hex).map_err(|error| {
            RepositoryError::InvalidRepository {
                reason: format!("commit '{ancestor_hex}' is not a valid object ID: {error}"),
            }
        })?;
        for _ in 0..10_000 {
            if oid == ancestor {
                return Ok(true);
            }
            let commit =
                git.find_commit(oid)
                    .map_err(|error| RepositoryError::InvalidRepository {
                        reason: format!("cannot read commit {oid}: {error}"),
                    })?;
            if commit.parent_count() == 0 {
                return Ok(false);
            }
            oid = commit
                .parent_id(0)
                .map_err(|error| RepositoryError::InvalidRepository {
                    reason: format!("cannot read first parent of {oid}: {error}"),
                })?;
        }
        Err(RepositoryError::InvalidRepository {
            reason: format!(
                "first-parent ancestry from {descendant_hex} exceeded 10,000 commits; refusing to guess the mapping"
            ),
        })
    }
}

fn write_head_map(
    root: &Path,
    mappings: &BTreeMap<String, HeadMapEntry>,
) -> Result<(), RepositoryError> {
    let path = root.join(HEAD_MAP_RELATIVE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| RepositoryError::InvalidRepository {
            reason: format!(
                "cannot create bridge directory '{}': {error}",
                parent.display()
            ),
        })?;
    }
    let wire = HeadMapWire {
        version: 1,
        mappings: mappings.clone(),
    };
    let mut bytes = serde_json::to_vec_pretty(&wire)
        .map_err(|error| RepositoryError::Serialization(error.to_string()))?;
    bytes.push(b'\n');
    atomic_write(&root.join(HEAD_MAP_RELATIVE), &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidRepository {
            reason: format!("target '{}' has no parent directory", path.display()),
        })?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("atomic"),
        std::process::id()
    ));
    let result = (|| -> std::io::Result<()> {
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| RepositoryError::InvalidRepository {
        reason: format!("cannot write '{}': {error}", path.display()),
    })
}

// ── Policy and helpers ───────────────────────────────────────────────────

/// The deterministic conversion policy for one live Git repository
/// (RFC §6.1–6.2): object format, platform capabilities, exclusion rules.
pub fn conversion_policy_for_git(
    git: &GitRepository,
) -> Result<ConversionPolicy, BridgeAnchorError> {
    let config = git.config().map_err(|error| {
        BridgeAnchorError::Git(format!("cannot read Git configuration: {error}"))
    })?;
    let object_format = match config.get_string("extensions.objectFormat") {
        Ok(value) if value.eq_ignore_ascii_case("sha1") => GitHashAlgorithm::Sha1,
        Ok(value) if value.eq_ignore_ascii_case("sha256") => GitHashAlgorithm::Sha256,
        Ok(value) => {
            return Err(BridgeAnchorError::Git(format!(
                "unsupported Git object algorithm '{value}'"
            )))
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => GitHashAlgorithm::Sha1,
        Err(error) => {
            return Err(BridgeAnchorError::Git(format!(
                "cannot read Git object algorithm: {error}"
            )))
        }
    };
    let mut policy = ConversionPolicy::new(object_format);
    policy.platform = PlatformCapabilities {
        lossless_unix_paths: cfg!(unix),
        symlinks: config.get_bool("core.symlinks").unwrap_or(cfg!(unix)),
        executable_bit: config.get_bool("core.filemode").unwrap_or(cfg!(unix)),
        case_sensitive: !config.get_bool("core.ignorecase").unwrap_or(false),
        unicode_normalizing: config.get_bool("core.precomposeunicode").unwrap_or(false),
    };
    Ok(policy)
}

fn object_format_for(oid: &git2::Oid) -> Result<GitObjectFormat, BridgeAnchorError> {
    match oid.as_bytes().len() {
        20 => Ok(GitObjectFormat::Sha1),
        32 => Ok(GitObjectFormat::Sha256),
        other => Err(BridgeAnchorError::Git(format!(
            "unexpected Git OID width {other}"
        ))),
    }
}

fn tagged_oid(oid: &git2::Oid) -> Result<GitOid, BridgeAnchorError> {
    let bytes = oid.as_bytes();
    match bytes.len() {
        20 => Ok(GitOid::Sha1(bytes.try_into().expect("sha1 width"))),
        32 => Ok(GitOid::Sha256(bytes.try_into().expect("sha256 width"))),
        other => Err(BridgeAnchorError::Git(format!(
            "unexpected Git OID width {other}"
        ))),
    }
}

/// One-line deterministic summary of an equivalence report for typed refusals.
pub fn format_equivalence_report(report: &crate::EquivalenceReport) -> String {
    report
        .mismatches
        .iter()
        .map(|mismatch| {
            let path = mismatch
                .path
                .as_ref()
                .map(|path| path.escaped())
                .unwrap_or_else(|| "<repository>".to_string());
            format!(
                "{:?}/{:?} {}: expected {}, actual {}",
                mismatch.layer, mismatch.kind, path, mismatch.expected, mismatch.actual
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

impl Repository {
    // ── AC-1: anchoring ──────────────────────────────────────────────────

    /// CB-7A AC-1: prove complete repository/index/worktree equivalence for
    /// `view` against Git HEAD under one conversion policy (RFC §6.1–6.2,
    /// §12.5). An empty mismatch list is the proof.
    pub fn bridge_anchor_equivalence(
        &self,
        git: &GitRepository,
        view: &str,
        policy: &ConversionPolicy,
    ) -> Result<crate::EquivalenceReport, BridgeAnchorError> {
        let project = self.project_tree(view, policy).map_err(|error| {
            BridgeAnchorError::Git(format!("cannot project view '{view}': {error}"))
        })?;
        let index = observe_git_index(&self.root, policy)?;
        let filter = crate::GitAttributesFilter::for_repository(&self.root);
        let worktree = super::observe_worktree(&self.root, Some(&index), &filter, policy)?;
        let claims = EquivalenceClaims {
            object_algorithm: Some(policy.object_format),
            git_tree_root: git
                .head()
                .ok()
                .and_then(|head| head.target())
                .and_then(|oid| git.find_commit(oid).ok())
                .map(|commit| commit.tree_id())
                .map(|oid| {
                    GitObjectId::new(policy.object_format, oid.as_bytes().to_vec())
                        .map_err(|error| BridgeAnchorError::Git(error.to_string()))
                })
                .transpose()?,
            ..EquivalenceClaims::default()
        };
        Ok(compare_project_state(
            &project, &index, &worktree, policy, &claims,
        ))
    }

    /// Refuse unless every layer is equivalent; classify pending work
    /// separately from structural divergence so remediation stays explicit
    /// (pending-work re-globalization belongs to CB-7B).
    fn anchor_equivalent_or_refuse(
        &self,
        git: &GitRepository,
        view: &str,
    ) -> Result<(), BridgeAnchorError> {
        let policy = conversion_policy_for_git(git)?;
        let report = self.bridge_anchor_equivalence(git, view, &policy)?;
        if report.is_equivalent() {
            return Ok(());
        }
        let pending_only = report.mismatches.iter().all(|mismatch| {
            matches!(
                mismatch.layer,
                EquivalenceLayer::GitIndex | EquivalenceLayer::Worktree
            )
        });
        let detail = format_equivalence_report(&report);
        if pending_only {
            Err(BridgeAnchorRefusal::DirtyState { detail }.into())
        } else {
            Err(BridgeAnchorRefusal::Mismatch { report: detail }.into())
        }
    }

    /// CB-7A AC-1: anchor the bridge by proving complete equivalence and
    /// creating the signed Anchor binding plus the verified working-copy
    /// checkpoint.
    ///
    /// The signature comes from an explicitly supplied key — no signing
    /// identity is wired here (managed provenance is CB-12B). Repeat enables
    /// with unchanged state are idempotent: an identical stored fact is
    /// reused, the create-only publication replays as a typed no-op, and the
    /// checkpoint is rewritten with the same evidence.
    ///
    /// CB-10B: `changes_pack` optionally attaches the bounded, pre-quarantined
    /// `changes.pack` fallback to the Anchor's binding tree (RFC §8.6) so a
    /// receiver without an Atomic remote can still verify the closure. The
    /// pack must be the output of [`crate::git_binding::assemble_changes_pack`];
    /// the publisher revalidates it and refuses private material closed.
    pub fn enable_bridge_anchor(
        &mut self,
        git: &GitRepository,
        signer: &atomic_identity::keypair::KeyPair,
    ) -> Result<BridgeAnchorOutcome, BridgeAnchorError> {
        self.enable_bridge_anchor_inner(git, signer, None)
    }

    /// [`Self::enable_bridge_anchor`] with a bounded `changes.pack` fallback
    /// attached to the Anchor's binding tree (CB-10B, RFC §8.6).
    pub fn enable_bridge_anchor_with_changes_pack(
        &mut self,
        git: &GitRepository,
        signer: &atomic_identity::keypair::KeyPair,
        changes_pack: &[u8],
    ) -> Result<BridgeAnchorOutcome, BridgeAnchorError> {
        if changes_pack.is_empty() {
            return Err(BridgeAnchorError::Repository(
                RepositoryError::InvalidOperation {
                    message: "an empty changes.pack cannot be published; omit it instead"
                        .to_string(),
                },
            ));
        }
        self.enable_bridge_anchor_inner(git, signer, Some(changes_pack))
    }

    fn enable_bridge_anchor_inner(
        &mut self,
        git: &GitRepository,
        signer: &atomic_identity::keypair::KeyPair,
        changes_pack: Option<&[u8]>,
    ) -> Result<BridgeAnchorOutcome, BridgeAnchorError> {
        // Typed HEAD classification first: unborn and detached HEADs refuse
        // before any equivalence work (RFC §7.1 step 4, Phase 7 tests).
        let observation = crate::observe_git_metadata(&self.root)?;
        let crate::WorkspaceGitObservation::Repository(observed) = &observation else {
            return Err(BridgeAnchorRefusal::UnbornHead {
                symref: "<no-git>".to_string(),
            }
            .into());
        };
        let view = match &observed.head {
            GitHeadObservation::Attached { .. } => {
                self.desired_view_name(self.require_working_copy_id()?)?
            }
            GitHeadObservation::Detached { oid } => {
                return Err(BridgeAnchorRefusal::DetachedHead { oid: oid.clone() }.into())
            }
            GitHeadObservation::Unborn { symref } => {
                return Err(BridgeAnchorRefusal::UnbornHead {
                    symref: symref.clone(),
                }
                .into())
            }
            GitHeadObservation::MissingTarget { symref } => {
                return Err(BridgeAnchorRefusal::MissingHeadTarget {
                    symref: symref.clone(),
                }
                .into())
            }
        };
        self.anchor_equivalent_or_refuse(git, &view)?;
        let binding = self.build_anchor_binding(git, &view, signer)?;
        let working_copy = self.require_working_copy_id()?;
        let publication = match changes_pack {
            Some(pack) => {
                self.publish_binding_with_changes_pack(working_copy, git, &binding, None, pack)?
            }
            None => self.publish_binding(working_copy, git, &binding, None)?,
        };
        let state = binding.payload().merkle_state;
        let git_head = binding.payload().git_commit.to_hex();
        let git_tree = binding.payload().git_tree.to_hex();
        // The verified checkpoint is written only after HEAD/index
        // re-observation (RFC §7.1 step 8, §7.2 step 6).
        self.write_verified_checkpoint(git, &view, &state)?;
        Ok(BridgeAnchorOutcome {
            binding,
            publication,
            view,
            state,
            git_head,
            git_tree,
        })
    }

    /// Build the signed Anchor payload for the current view/HEAD pair,
    /// reusing a byte-identical stored fact on repeat enables (RFC §5.1).
    fn build_anchor_binding(
        &self,
        git: &GitRepository,
        view: &str,
        signer: &atomic_identity::keypair::KeyPair,
    ) -> Result<GitStateBinding, BridgeAnchorError> {
        let head = git
            .head()
            .ok()
            .and_then(|head| head.target())
            .ok_or_else(|| {
                BridgeAnchorError::Git("Git HEAD is not a direct commit reference".to_string())
            })?;
        let commit = git.find_commit(head).map_err(|error| {
            BridgeAnchorError::Git(format!("cannot read Git HEAD commit: {error}"))
        })?;
        let raw_commit = git
            .odb()
            .map_err(|error| BridgeAnchorError::Git(format!("cannot open Git odb: {error}")))?
            .read(head)
            .map_err(|error| {
                BridgeAnchorError::Git(format!("cannot read raw HEAD commit: {error}"))
            })?
            .data()
            .to_vec();

        let identity = self.view_identity(view)?;
        let ordered_changes: Vec<atomic_core::Hash> = self
            .effective_history(Some(view))?
            .iter()
            .map(|entry| entry.hash)
            .collect();
        let operation = self.sole_working_copy_operation_head()?;

        let mut payload = GitStateBindingPayload {
            version: BINDING_VERSION,
            git_object_format: object_format_for(&head)?,
            git_commit: tagged_oid(&head)?,
            git_tree: tagged_oid(&commit.tree_id())?,
            git_parents: commit
                .parent_ids()
                .map(|parent| tagged_oid(&parent))
                .collect::<Result<Vec<_>, _>>()?,
            raw_commit_object: Some(raw_commit),
            set_id: identity.set_id,
            merkle_state: identity.merkle,
            view_hint: Some(view.to_string()),
            ordered_changes,
            closure_root: atomic_core::Hash::from_bytes([0u8; 32]),
            operation,
            origin: CausalOrigin::ExactAtomicResurrection,
            loss: Vec::new(),
            provenance_roots: Vec::new(),
            attestation_roots: Vec::new(),
            signer: BindingSigner::for_keypair(signer),
        };
        payload.closure_root = payload.compute_closure_root();

        // Idempotent retries: an identical stored fact is reused byte-for-byte.
        if let Some(existing) = self.find_stored_identical_binding(&payload, signer)? {
            return Ok(existing);
        }
        GitStateBinding::sign(payload, signer).map_err(|error| {
            BridgeAnchorError::Repository(RepositoryError::InvalidOperation {
                message: format!("cannot sign the Anchor binding: {error}"),
            })
        })
    }

    /// Find a stored binding stating the same fact under the same signer:
    /// identical retries republish exactly those bytes (RFC §5.1 immutability).
    fn find_stored_identical_binding(
        &self,
        payload: &GitStateBindingPayload,
        signer: &atomic_identity::keypair::KeyPair,
    ) -> Result<Option<GitStateBinding>, RepositoryError> {
        let my_did = atomic_canonical::did::did_for_public_key(&signer.public);
        for id in self.binding_ids()? {
            let Some(binding) = self.load_binding(&id)? else {
                continue;
            };
            let stored = binding.payload();
            let same_fact = stored.git_object_format == payload.git_object_format
                && stored.git_commit == payload.git_commit
                && stored.git_tree == payload.git_tree
                && stored.git_parents == payload.git_parents
                && stored.set_id == payload.set_id
                && stored.merkle_state == payload.merkle_state
                && stored.view_hint == payload.view_hint
                && stored.ordered_changes == payload.ordered_changes
                && stored.closure_root == payload.closure_root
                && stored.origin == payload.origin
                && stored.loss == payload.loss
                && stored.signer.did == my_did
                && stored.signer.did == payload.signer.did;
            if same_fact {
                return Ok(Some(binding));
            }
        }
        Ok(None)
    }

    /// The sole current working-copy operation head — the Anchor's `operation`
    /// link (RFC §5.1).
    fn sole_working_copy_operation_head(
        &self,
    ) -> Result<atomic_core::types::OperationId, RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        let log = self.operation_log(
            atomic_core::operation::OperationScope::WorkingCopy(working_copy),
            Some(1),
            false,
        )?;
        match log.head_state {
            crate::OperationHeadState::Single(head) => Ok(head),
            crate::OperationHeadState::Empty => Err(RepositoryError::InvalidOperation {
                message: "no causal operation exists to anchor; record or import state first"
                    .to_string(),
            }),
            crate::OperationHeadState::Diverged(heads) => {
                Err(RepositoryError::OperationHeadsDiverged {
                    scope: "working copy".to_string(),
                    heads: heads.iter().map(ToString::to_string).collect(),
                })
            }
        }
    }

    /// Write the version-2 verified checkpoint after re-observing HEAD and
    /// confirming the view state (RFC §7.2 step 6, §7.1 step 8). Detached
    /// HEADs are supported: the checkpoint records a `None` symref so
    /// recording on an ephemeral `git/<oid>` view stays aligned.
    pub(crate) fn write_verified_checkpoint(
        &self,
        git: &GitRepository,
        view: &str,
        expected_state: &Merkle,
    ) -> Result<WorkspaceCheckpoint, RepositoryError> {
        let checkpoint = self.derive_workspace_checkpoint(git, view, expected_state)?;
        write_workspace_checkpoint(self.root(), &checkpoint)?;
        Ok(checkpoint)
    }

    /// Derive (without writing) the version-2 checkpoint for one verified
    /// view state (R3: the completion operation's Checkpoint lease derives
    /// the expected bytes at prepare time and re-derives them at execute
    /// time — a moved HEAD or index between the two fails the lease).
    pub fn derive_workspace_checkpoint(
        &self,
        git: &GitRepository,
        view: &str,
        expected_state: &Merkle,
    ) -> Result<WorkspaceCheckpoint, RepositoryError> {
        let info = self.get_view_info(view)?;
        if info.state != *expected_state {
            return Err(RepositoryError::InvalidRepository {
                reason: format!(
                    "view '{view}' moved while the checkpoint was being written: expected {}, observed {}",
                    expected_state, info.state
                ),
            });
        }
        let (head_hex, head_tree, symref) = {
            let head = git
                .head()
                .map_err(|error| RepositoryError::InvalidRepository {
                    reason: format!("cannot re-observe Git HEAD for the checkpoint: {error}"),
                })?;
            let oid = head
                .target()
                .ok_or_else(|| RepositoryError::InvalidRepository {
                    reason: "Git HEAD does not point directly at a commit".to_string(),
                })?;
            let commit =
                git.find_commit(oid)
                    .map_err(|error| RepositoryError::InvalidRepository {
                        reason: format!("cannot read Git HEAD commit for the checkpoint: {error}"),
                    })?;
            // A detached HEAD's reference name is literally "HEAD"; the
            // checkpoint records a `None` symref for detached states so the
            // entry protocol keeps them aligned (RFC §7.5).
            let symref = if head.is_branch() {
                head.name().map(str::to_string)
            } else {
                None
            };
            (oid.to_string(), commit.tree_id().to_string(), symref)
        };
        let index_digest = match crate::observe_git_metadata(&self.root) {
            Ok(super::WorkspaceGitObservation::Repository(observed)) => {
                Some(observed.index_digest.to_base32())
            }
            _ => None,
        };
        let checkpoint = WorkspaceCheckpoint {
            version: 2,
            view: view.to_string(),
            atomic_state: expected_state.to_base32(),
            git_head_symref: symref,
            git_head: head_hex,
            git_tree: head_tree,
            // R2 (CB-7B): the checkpoint binds the observed primary-index
            // digest so pre-transition evidence captured against this
            // checkpoint is authenticated against real index facts, not
            // `None` placeholders (alternate-index evidence stays detectable).
            git_index_tree: None,
            git_index_digest: index_digest,
        };
        Ok(checkpoint)
    }

    // ── AC-2 / AC-3: bound HEAD adoption ─────────────────────────────────

    /// CB-7A §7.3: reconcile a changed or detached Git HEAD against a verified
    /// binding before any filesystem scanning (RFC §12.2).
    ///
    /// Called from the workspace entry protocol with the ordered operation
    /// lock held. Adoption never writes tracked files: it resurrects the
    /// binding (journaled, receipted), verifies the stage-0 index and worktree
    /// are clean relative to the new HEAD, advances the working-copy record
    /// under a leased `ImportGitHead` operation, re-observes HEAD/index, and
    /// only then rewrites the verified checkpoint and head map.
    pub(super) fn adopt_bound_git_head_locked(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        working_copy: WorkingCopyId,
        checkpoint: Option<&WorkspaceCheckpoint>,
        observation: &super::WorkspaceGitObservation,
    ) -> Result<AdoptBoundHead, RepositoryError> {
        let super::WorkspaceGitObservation::Repository(git_observation) = observation else {
            return Err(RepositoryError::HeadAdoptionRefused {
                head: "<no-git>".to_string(),
                reason: "the workspace has no Git repository to adopt from".to_string(),
            });
        };
        let (head_hex, symref) = match &git_observation.head {
            GitHeadObservation::Attached { symref, oid } => (oid.clone(), Some(symref.clone())),
            GitHeadObservation::Detached { oid } => (oid.clone(), None),
            GitHeadObservation::Unborn { symref } => {
                return Err(RepositoryError::HeadAdoptionRefused {
                    head: format!("unborn:{symref}"),
                    reason: "an unborn HEAD has no commit to adopt; create a commit in Git"
                        .to_string(),
                })
            }
            GitHeadObservation::MissingTarget { symref } => {
                return Err(RepositoryError::HeadAdoptionRefused {
                    head: format!("missing:{symref}"),
                    reason: "the HEAD symbol points at a missing branch; restore it in Git"
                        .to_string(),
                })
            }
        };

        // Aligned short-circuit: nothing to adopt when the checkpoint already
        // matches this observation exactly.
        if let Some(checkpoint) = checkpoint {
            let same_head = checkpoint.git_head == head_hex
                && checkpoint.git_head_symref == symref
                && git_observation.head_tree.as_deref() == Some(checkpoint.git_tree.as_str());
            if same_head {
                return Ok(AdoptBoundHead::Aligned);
            }
        }

        let git = git2::Repository::discover(&self.root).map_err(|error| {
            RepositoryError::HeadAdoptionRefused {
                head: head_hex.clone(),
                reason: format!("cannot open the Git repository for adoption: {error}"),
            }
        })?;
        let oid = git2::Oid::from_str(&head_hex).map_err(|error| {
            RepositoryError::HeadAdoptionRefused {
                head: head_hex.clone(),
                reason: format!("HEAD is not a valid Git object ID: {error}"),
            }
        })?;
        let tagged_head = {
            let algorithm = match head_hex.len() {
                40 => GitHashAlgorithm::Sha1,
                64 => GitHashAlgorithm::Sha256,
                other => {
                    return Err(RepositoryError::HeadAdoptionRefused {
                        head: head_hex.clone(),
                        reason: format!("unsupported Git OID width {other}"),
                    })
                }
            };
            GitObjectId::new(algorithm, oid.as_bytes().to_vec()).map_err(|error| {
                RepositoryError::HeadAdoptionRefused {
                    head: head_hex.clone(),
                    reason: error.to_string(),
                }
            })?
        };

        // §7.3 step 2 / §7.5: map the commit to its target view.
        //
        // Same-commit branch attach (`git switch -c <name>` from an ephemeral
        // view): HEAD is unchanged, so the workspace state needs no
        // resurrection or clean gate — the §7.5 rename maps the ephemeral
        // view onto the new branch and preserves its identity.
        if checkpoint.is_some_and(|checkpoint| {
            checkpoint.git_head == head_hex && checkpoint.git_head_symref != symref
        }) {
            if let Some(renamed) =
                self.try_rename_ephemeral_for_new_branch(&head_hex, symref.as_deref())?
            {
                self.write_verified_checkpoint(&git, &renamed.view, &renamed.state)?;
                let mut mappings = read_head_map(self.root())?;
                mappings.insert(
                    head_hex.clone(),
                    HeadMapEntry {
                        view: renamed.view.clone(),
                        ephemeral: false,
                    },
                );
                write_head_map(self.root(), &mappings)?;
                return Ok(AdoptBoundHead::Adopted {
                    view: renamed.view,
                    state: renamed.state,
                    ephemeral: false,
                    import: None,
                    resurrect: None,
                });
            }
        }

        // §7.3: a known binding is resurrected, never re-synthesized (§12.4);
        // an unbound commit is a typed refusal (unbound adoption ships with
        // Phase 9; the prototype importer is never invoked here).
        let binding = match self.resolve_git_sha(&git, &tagged_head)? {
            crate::GitShaResolution::VerifiedBinding { binding, .. } => binding,
            crate::GitShaResolution::Cold => {
                return Err(RepositoryError::HeadAdoptionUnbound {
                    head: head_hex.clone(),
                    reason: "no verified binding covers this commit; unbound Git adoption \
                             stays disabled until Phase 9 (CB-9B/9C) — bind it explicitly \
                             with 'atomic git bridge binding publish --key-file …'"
                        .to_string(),
                })
            }
            crate::GitShaResolution::Unresolved { detail } => {
                return Err(RepositoryError::HeadAdoptionUnbound {
                    head: head_hex.clone(),
                    reason: format!("binding candidates failed verification: {detail}"),
                })
            }
        };

        // §7.3 step 2 / §7.5: map the commit to its target view.
        let target = self.resolve_head_target(&git, &binding, symref.as_deref(), &head_hex)?;
        if let Some(old_name) = &target.rename_from {
            self.rename_view_transaction(old_name, &target.view)?;
        } else if !self.view_exists(&target.view)? {
            self.create_adoption_view(&target)?;
        }

        // Known bindings are resurrected, never re-synthesized (§12.4).
        let resurrection: ExactResurrection = self
            .resurrect_binding_exact_locked(operation_lock, &git, &binding, &target.view, None)
            .map_err(|error| match error {
                RepositoryError::ResurrectionContended { id, attempts } => {
                    RepositoryError::HeadAdoptionRefused {
                        head: head_hex.clone(),
                        reason: format!(
                            "resurrection of binding {id} contended with concurrent Git \
                             mutation for {attempts} attempts"
                        ),
                    }
                }
                other => other,
            })?;

        // Clean-adoption gate (§7.3): a stage-0 index and a worktree clean
        // relative to the new HEAD under the current conversion policy. Full
        // manifest equivalence is verified, not touched paths (§12.5).
        let state = binding.payload().merkle_state;
        let policy = conversion_policy_for_git(&git).map_err(|error| {
            RepositoryError::HeadAdoptionRefused {
                head: head_hex.clone(),
                reason: error.to_string(),
            }
        })?;
        let report = self
            .bridge_anchor_equivalence(&git, &target.view, &policy)
            .map_err(|error| RepositoryError::HeadAdoptionRefused {
                head: head_hex.clone(),
                reason: error.to_string(),
            })?;
        if !report.is_equivalent() {
            // Post-adoption capture discipline: a previous adoption attempt
            // that already published its replacement snapshot against THIS
            // HEAD (crash before the checkpoint publish) must not be
            // re-captured as unknown pre-checkout origin — the published
            // snapshot already carries the truthful attribution and covers
            // the worktree.
            let already_published =
                self.adoption_snapshot_already_published(working_copy, &head_hex)?;
            if !already_published {
                // CB-7B §7.3 branches 2–3: with authenticated pre-transition
                // evidence the differences are a known carried edit re-assembled
                // against the new baseline; without proof they become a fresh
                // opaque snapshot marked unknown pre-checkout attribution. Both
                // branches are journaled and verified; any failure preserves the
                // prior snapshot and evidence and returns the typed refusal.
                let evidence = super::adoption::read_pre_transition_evidence(self.root())?;
                let proven = evidence
                    .as_ref()
                    .filter(|evidence| {
                        self.pre_transition_evidence_proves(
                            evidence,
                            checkpoint.expect("clean gate implies a checkpoint exists"),
                            working_copy,
                            &policy,
                            observation,
                        )
                    })
                    .cloned();
                let outcome = match proven {
                    Some(evidence) => match self.adopt_known_edit_locked(
                        operation_lock,
                        working_copy,
                        &evidence,
                        checkpoint.expect("clean gate implies a checkpoint exists"),
                        &policy,
                        &target.view,
                        state,
                        &head_hex,
                    ) {
                        Ok(outcome) => outcome,
                        // R2: the pre-captured materialization no longer
                        // describes the worktree (newer post-capture edits or
                        // unexplained extra files). The known attribution is
                        // unsupported — fall back to the unknown-origin branch,
                        // which preserves the newer bytes and never reattributes
                        // them. Nothing was mutated by the refused branch.
                        Err(super::adoption::AdoptKnownEditError::CarriedStateUnproven(detail)) => {
                            log::info!(
                                "bound HEAD adoption: captured materialization no longer proves \
                                 the worktree; falling back to unknown pre-checkout origin: {detail}"
                            );
                            self.adopt_unknown_origin_locked(
                                operation_lock,
                                working_copy,
                                checkpoint.expect("clean gate implies a checkpoint exists"),
                                &target.view,
                                state,
                                &head_hex,
                            )?
                        }
                        Err(super::adoption::AdoptKnownEditError::Repository(error)) => {
                            return Err(error)
                        }
                    },
                    None => self.adopt_unknown_origin_locked(
                        operation_lock,
                        working_copy,
                        checkpoint.expect("clean gate implies a checkpoint exists"),
                        &target.view,
                        state,
                        &head_hex,
                    )?,
                };
                log::info!(
                    "bound HEAD adoption captured {:?} differences as snapshot {:?} (operation {})",
                    outcome.origin,
                    outcome.snapshot.map(|hash| hash.to_base32()),
                    outcome.reassembly
                );
            }
        }

        // Adopt: advance the working-copy record under a leased ImportGitHead
        // operation. The projected state already equals the bound tree, so no
        // tracked file is written (§7.3 "adopt: no file writes"); the CB-7B
        // dirty branches above already advanced the record and the advance
        // below is then an idempotent no-op.
        let view_id = {
            let txn = self.pristine.read_txn().map_err(pristine_map)?;
            txn.get_view(&target.view)
                .map_err(pristine_map)?
                .ok_or(RepositoryError::ViewNotFound {
                    name: target.view.clone(),
                })?
                .id
        };
        let before_record = self.working_copy_record(working_copy)?;
        let mut after_record = before_record.clone();
        after_record.desired_view = view_id;
        after_record.desired_state = state;
        after_record.materialized_state = Some(state);
        let import = if working_copy_state_ref(before_record.clone())
            == working_copy_state_ref(after_record.clone())
        {
            None
        } else {
            // R4: when the adoption also changes views, ignored artifacts
            // swap shelves under the same collision-safe planner used by the
            // dirty branches (old shelf / current disk / target shelf).
            let mut shelf_effects = Vec::new();
            let shelf = match checkpoint.filter(|checkpoint| checkpoint.view != target.view) {
                Some(checkpoint) => self.plan_adoption_shelf_effects(
                    operation_lock,
                    working_copy,
                    &checkpoint.view,
                    &target.view,
                    &mut shelf_effects,
                )?,
                None => super::adoption::AdoptionShelfPlan::default(),
            };
            Some(self.journal_import_git_head(
                operation_lock,
                working_copy,
                &target.view,
                state,
                before_record,
                after_record,
                atomic_core::Hash::of(binding.encode().as_slice()),
                shelf_effects,
                shelf,
            )?)
        };

        // §7.1 step 8: only a stable HEAD/index observation may publish the
        // verified checkpoint.
        let reobserved = crate::observe_git_metadata(&self.root).map_err(observation_map)?;
        if reobserved.token() != observation.token() {
            return Err(RepositoryError::HeadAdoptionRefused {
                head: head_hex.clone(),
                reason: "Git changed while the bound state was being adopted; retry against \
                         the new observation"
                    .to_string(),
            });
        }
        // ac-4 failpoint — opt-in test instrumentation (feature
        // `adoption-test-injection`, R8): crash after adoption completes,
        // before the verified checkpoint is published. Reopen must re-run
        // the (idempotent) adoption against the retained WIP ref, snapshot,
        // and evidence. Shipping builds compile no check at all.
        #[cfg(feature = "adoption-test-injection")]
        if std::env::var_os("ATOMIC_FAIL_ADOPTION_BEFORE_CHECKPOINT").is_some() {
            return Err(RepositoryError::Io(std::io::Error::other(
                "debug failpoint: ATOMIC_FAIL_ADOPTION_BEFORE_CHECKPOINT",
            )));
        }
        self.write_verified_checkpoint(&git, &target.view, &state)?;
        let mut mappings = read_head_map(self.root())?;
        mappings.insert(
            head_hex.clone(),
            HeadMapEntry {
                view: target.view.clone(),
                ephemeral: target.ephemeral,
            },
        );
        write_head_map(self.root(), &mappings)?;

        Ok(AdoptBoundHead::Adopted {
            view: target.view,
            state,
            ephemeral: target.ephemeral,
            import,
            resurrect: resurrection.operation,
        })
    }

    /// The `git switch -c <name>` fast path (§7.5): when the workspace sits
    /// on the ephemeral view mapped to the current (unchanged) commit and the
    /// observed HEAD just became attached to a newly created branch, rename
    /// that ephemeral view onto the branch and keep its identity.
    ///
    /// Returns `Ok(None)` when the preconditions do not hold and the caller
    /// must run the full adoption path.
    fn try_rename_ephemeral_for_new_branch(
        &self,
        head_hex: &str,
        symref: Option<&str>,
    ) -> Result<Option<RenamedAdoption>, RepositoryError> {
        let Some(branch) = symref.and_then(|symref| symref.strip_prefix("refs/heads/")) else {
            return Ok(None);
        };
        let mappings = read_head_map(self.root())?;
        let Some(entry) = mappings.get(head_hex) else {
            return Ok(None);
        };
        if !entry.ephemeral || !self.view_exists(&entry.view)? || entry.view == branch {
            return Ok(None);
        }
        let working_copy = self.require_working_copy_id()?;
        let desired_view = self.desired_view_name(working_copy)?;
        if desired_view != entry.view {
            // The workspace is not on the ephemeral view; a branch-attach is
            // an ordinary mapping decision, not a §7.5 rename.
            return Ok(None);
        }
        let git = git2::Repository::discover(self.root()).map_err(|error| {
            RepositoryError::HeadAdoptionRefused {
                head: head_hex.to_string(),
                reason: format!("cannot open the Git repository: {error}"),
            }
        })?;
        if !branch_is_newly_created(&git, branch) {
            return Ok(None);
        }
        if self.view_exists(branch)? {
            return Err(RepositoryError::HeadAdoptionRefused {
                head: head_hex.to_string(),
                reason: format!(
                    "cannot map new branch '{branch}' onto the ephemeral view '{}': a view \
                     with that name already exists; explicit branch/view reconciliation is \
                     CB-10A",
                    entry.view
                ),
            });
        }
        let state = {
            let txn = self.pristine.read_txn().map_err(pristine_map)?;
            txn.get_view(&entry.view)
                .map_err(pristine_map)?
                .ok_or(RepositoryError::ViewNotFound {
                    name: entry.view.clone(),
                })?
                .state
        };
        self.rename_view_transaction(&entry.view, branch)?;
        Ok(Some(RenamedAdoption {
            view: branch.to_string(),
            state,
        }))
    }

    /// Resolve the adoption target view for one bound commit (§7.5, §8.1).
    fn resolve_head_target(
        &self,
        git: &GitRepository,
        binding: &GitStateBinding,
        symref: Option<&str>,
        head_hex: &str,
    ) -> Result<HeadTarget, RepositoryError> {
        let hinted = binding.payload().view_hint.clone();
        let hinted_shared_parent = |hint: &Option<String>| -> Option<String> {
            hint.as_ref()
                .and_then(|hint| self.get_view_info(hint).ok())
                .filter(|info| info.scope == ViewScope::Shared)
                .map(|info| info.name)
        };

        if let Some(branch) = symref.and_then(|symref| symref.strip_prefix("refs/heads/")) {
            // `git switch -c` / `git checkout -b` (§7.5): an attached HEAD on
            // a newly created branch whose commit maps to an ephemeral view
            // renames that view instead of discarding its identity.
            if let Some(entry) = read_head_map(self.root())?.get(head_hex) {
                if entry.ephemeral
                    && self.view_exists(&entry.view)?
                    && entry.view != branch
                    && branch_is_newly_created(git, branch)
                {
                    if self.view_exists(branch)? {
                        return Err(RepositoryError::HeadAdoptionRefused {
                            head: head_hex.to_string(),
                            reason: format!(
                                "cannot map new branch '{branch}' onto the ephemeral view \
                                 '{}': a view with that name already exists; explicit \
                                 branch/view reconciliation is CB-10A",
                                entry.view
                            ),
                        });
                    }
                    return Ok(HeadTarget {
                        view: branch.to_string(),
                        ephemeral: false,
                        rename_from: Some(entry.view.clone()),
                        parent: None,
                    });
                }
            }
            // The binding's view_hint is the mapped view for the binding
            // (§7.3); a same-name view is never authority on its own.
            if let Some(hint) = &hinted {
                return Ok(HeadTarget {
                    view: hint.clone(),
                    ephemeral: false,
                    rename_from: None,
                    parent: None,
                });
            }
        }

        if let Some(entry) = read_head_map(self.root())?.get(head_hex) {
            if self.view_exists(&entry.view)? {
                return Ok(HeadTarget {
                    view: entry.view.clone(),
                    ephemeral: entry.ephemeral,
                    rename_from: None,
                    parent: None,
                });
            }
        }

        // Detached HEAD, or no usable named mapping: ephemeral Draft
        // `git/<short-oid>` with the nearest hinted Shared parent or none.
        let name = self.ephemeral_view_name(head_hex)?;
        Ok(HeadTarget {
            view: name,
            ephemeral: true,
            rename_from: None,
            parent: hinted_shared_parent(&hinted),
        })
    }

    /// Create the view an adoption targets: an ephemeral Draft with optional
    /// parent, or the missing binding-hinted view as Shared.
    ///
    /// CB-7B: an ephemeral adoption view never inherits a Shared parent's
    /// change closure. Its projection must equal exactly the adopted
    /// binding's tree (§7.3 verification); inheriting an unrelated Shared
    /// closure would fabricate baseline content the binding never contained.
    /// The §7.5 parent hint stays a mapping/retention concern (CB-10A/CB-13).
    fn create_adoption_view(&mut self, target: &HeadTarget) -> Result<(), RepositoryError> {
        if !target.ephemeral {
            return self.create_view_with_identity(&target.view, ViewScope::Shared, None);
        }
        self.create_view_with_identity(&target.view, ViewScope::Draft, None)
    }

    /// Deterministic ephemeral view name `git/<short-oid>` (RFC §7.5) with
    /// short-OID collision handling: the prefix grows one character at a time
    /// while a different commit owns the name; exhausting the full object ID
    /// is a typed refusal.
    fn ephemeral_view_name(&self, head_hex: &str) -> Result<String, RepositoryError> {
        let lower = head_hex.to_ascii_lowercase();
        let mut prefix = EPHEMERAL_MIN_PREFIX.min(lower.len());
        loop {
            if prefix > lower.len() {
                return Err(RepositoryError::HeadAdoptionRefused {
                    head: head_hex.to_string(),
                    reason: "short-OID collision exhausted the full object ID".to_string(),
                });
            }
            let candidate = format!("git/{}", &lower[..prefix]);
            match self.get_view_info(&candidate) {
                Err(RepositoryError::ViewNotFound { .. }) => return Ok(candidate),
                Err(error) => return Err(error),
                Ok(_info) => {
                    let mappings = read_head_map(self.root())?;
                    let same_commit = mappings
                        .get(&lower)
                        .is_some_and(|entry| entry.view == candidate)
                        || mappings.iter().any(|(oid, entry)| {
                            entry.view == candidate && oid.eq_ignore_ascii_case(&lower)
                        });
                    if same_commit {
                        return Ok(candidate);
                    }
                    prefix += 1;
                }
            }
        }
    }

    /// Journal the `ImportGitHead` working-copy adoption (RFC §7.3, §12.2):
    /// a leased working-copy record transition, journaled before the record
    /// changes, with the adopted binding as evidence.
    #[allow(clippy::too_many_arguments)]
    fn journal_import_git_head(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        working_copy: WorkingCopyId,
        view: &str,
        state: Merkle,
        before_record: atomic_core::pristine::WorkingCopyRecord,
        after_record: atomic_core::pristine::WorkingCopyRecord,
        evidence: atomic_core::Hash,
        shelf_effects: Vec<EffectPlan>,
        shelf: super::adoption::AdoptionShelfPlan,
    ) -> Result<atomic_core::types::OperationId, RepositoryError> {
        let before_wc = working_copy_state_ref(before_record);
        let after_wc = working_copy_state_ref(after_record);
        let state_ref = RepoStateRef {
            view: Some(ViewStateRef {
                name: view.to_string(),
                state,
                set_id: None,
            }),
            working_copy: Some(before_wc.clone()),
            git: None,
        };
        let after_state = RepoStateRef {
            view: state_ref.view.clone(),
            working_copy: Some(after_wc.clone()),
            git: None,
        };
        // R4: a clean adoption is still a view transition — its ignored
        // artifacts swap shelves through the same collision-safe planner and
        // executor as the dirty branches. The planned shelf effects keep
        // their ordinals; the record advance is the last effect.
        let mut effects = shelf_effects;
        effects.push(EffectPlan {
            ordinal: effects.len() as u32,
            target: EffectTarget::WorkingCopy { working_copy },
            expected_old: EffectValue::WorkingCopy(before_wc),
            expected_new: EffectValue::WorkingCopy(after_wc.clone()),
        });
        let prepared = self.prepare_working_copy_transition(
            operation_lock,
            OperationKind::ImportGitHead,
            None,
            state_ref,
            after_state,
            effects,
            vec![evidence],
            ActorRef::System {
                name: "git-head-adoption".to_string(),
            },
            current_operation_timestamp_ms(),
        )?;
        let operation_id = prepared.operation().id();

        // R4: execute the shelf transitions under their leases before the
        // record advance (third values fail closed).
        let execute_result = (|| -> Result<(), RepositoryError> {
            for path in &shelf.ignored {
                self.execute_shelve_path(
                    operation_lock,
                    &prepared,
                    &shelf.old_view.clone().unwrap_or_default(),
                    path,
                )?;
            }
            for path in &shelf.restored {
                self.execute_restore_path(operation_lock, &prepared, view, path)?;
            }
            Ok(())
        })();
        if let Err(error) = execute_result {
            let _ = self.recover_incomplete_operation(operation_lock);
            return Err(error);
        }

        let target = EffectTarget::WorkingCopy { working_copy };
        let observed_before = self.observe_operation_effect(working_copy, &target)?;
        self.apply_working_copy_state_locked(operation_lock, &after_wc)?;
        let observed_after = self.observe_operation_effect(working_copy, &target)?;
        let advance_ordinal = prepared
            .operation()
            .payload()
            .delta
            .effects
            .iter()
            .find(|effect| effect.target == target)
            .map(|effect| effect.ordinal)
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: "import operation lost its record-advance effect".to_string(),
            })?;
        self.record_effect_outcome(
            operation_lock,
            operation_id,
            advance_ordinal,
            observed_before,
            observed_after,
        )?;
        self.finalize_operation_verified(operation_lock, operation_id)?;
        Ok(operation_id)
    }

    /// Rename a Draft view in one atomic transaction, preserving its change
    /// log, Merkle chain, tags, conflicts, and every working-copy pointer
    /// (RFC §7.5 `git switch -c`, §8.1 rename reconciliation).
    ///
    /// The rename facts are recorded as `ImportGitHead` evidence by the
    /// adoption that requests them; routing renames through dedicated
    /// metadata leases and remote/shared reconciliation is CB-10A. The
    /// transaction is idempotent under retry: it either completes wholly or
    /// leaves the old view untouched.
    pub(super) fn rename_view_transaction(
        &self,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), RepositoryError> {
        if old_name == new_name {
            return Ok(());
        }
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let old =
            txn.get_view(old_name)
                .map_err(pristine_map)?
                .ok_or(RepositoryError::ViewNotFound {
                    name: old_name.to_string(),
                })?;
        if txn.get_view(new_name).map_err(pristine_map)?.is_some() {
            return Err(RepositoryError::ViewAlreadyExists {
                name: new_name.to_string(),
            });
        }
        if old.kind != ViewScope::Draft {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "cannot rename view '{old_name}': only Draft views rename through \
                     adoption; shared-view reconciliation is CB-10A"
                ),
            });
        }
        let mut new_state = txn
            .create_view(new_name, old.kind, old.parent)
            .map_err(pristine_map)?;
        // Collect the change log first: the iteration borrows `txn`
        // immutably while `put_change` below mutates it.
        let mut log: Vec<(u64, atomic_core::types::NodeId, atomic_core::Hash)> = Vec::new();
        for row in txn.iter_changes(&old, 0).map_err(pristine_map)? {
            let (seq, change_id, _state) = row.map_err(pristine_map)?;
            let hash = txn
                .get_external(change_id)
                .map_err(pristine_map)?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "change {} in view '{old_name}' has no external hash",
                        change_id.get()
                    ))
                })?;
            log.push((seq, change_id, hash));
        }
        for (seq, change_id, hash) in log {
            let copied = txn
                .put_change(&mut new_state, change_id, &hash)
                .map_err(pristine_map)?;
            if copied != seq {
                return Err(RepositoryError::InvalidRepository {
                    reason: format!(
                        "renaming '{old_name}' produced sequence {copied} where {seq} was \
                         expected"
                    ),
                });
            }
        }
        if new_state.state != old.state || new_state.change_count != old.change_count {
            return Err(RepositoryError::InvalidRepository {
                reason: format!(
                    "renaming '{old_name}' to '{new_name}' did not preserve identity (state \
                     {} vs {}, count {} vs {})",
                    new_state.state, old.state, new_state.change_count, old.change_count
                ),
            });
        }
        txn.update_view(&new_state).map_err(pristine_map)?;

        // Private snapshot views (`wc/<working-copy>`) follow the workspace:
        // they re-parent onto the renamed view in the same transaction
        // (CB-7B), while any other child keeps the CB-10A refusal.
        for child in txn.get_children_views(old.id).map_err(pristine_map)? {
            if !child.name.starts_with("wc/") {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot rename view '{old_name}': it has child views; reconciliation is \
                         CB-10A"
                    ),
                });
            }
            let mut reparented = child.clone();
            reparented.parent = Some(new_state.id);
            txn.update_view(&reparented).map_err(pristine_map)?;
        }

        // Tags and persisted conflicts carry the view identity; copy them.
        for tag in txn.list_tags(old_name).map_err(pristine_map)? {
            let mut tag = tag.clone();
            tag.view = new_name.to_string();
            txn.put_tag(&tag).map_err(pristine_map)?;
        }
        for (inode, conflicts) in txn.iter_conflicts(old.id).map_err(pristine_map)? {
            txn.put_conflicts(new_state.id, inode, &conflicts)
                .map_err(pristine_map)?;
        }

        // Repoint working copies before deleting the old view row.
        for mut record in txn.list_working_copies().map_err(pristine_map)? {
            if record.desired_view == old.id {
                record.desired_view = new_state.id;
                txn.put_working_copy(&record).map_err(pristine_map)?;
            }
        }
        txn.del_view(&old).map_err(pristine_map)?;
        txn.commit().map_err(pristine_map)?;
        Ok(())
    }
}

/// Read-only pre-check for the entry boundary: does a verified binding cover
/// the observed Git HEAD? A `None` answer (no Git, unbound commit, unborn or
/// missing HEAD) refuses adoption before any recovery mutation, keeping typed
/// refusals non-mutating (RFC §12.11).
pub(super) fn head_binding_for_observation(
    repository: &Repository,
    observation: &super::WorkspaceGitObservation,
) -> Option<crate::git_binding::GitStateBinding> {
    let super::WorkspaceGitObservation::Repository(git_observation) = observation else {
        return None;
    };
    let head_hex = head_oid_hex(&git_observation.head)?.to_string();
    let git = git2::Repository::discover(repository.root()).ok()?;
    let oid = git2::Oid::from_str(&head_hex).ok()?;
    let algorithm = match head_hex.len() {
        40 => GitHashAlgorithm::Sha1,
        64 => GitHashAlgorithm::Sha256,
        _ => return None,
    };
    let tagged = GitObjectId::new(algorithm, oid.as_bytes().to_vec()).ok()?;
    match repository.resolve_git_sha(&git, &tagged) {
        Ok(crate::GitShaResolution::VerifiedBinding { binding, .. }) => Some(binding),
        _ => None,
    }
}

fn head_oid_hex(head: &GitHeadObservation) -> Option<&str> {
    match head {
        GitHeadObservation::Attached { oid, .. } | GitHeadObservation::Detached { oid } => {
            Some(oid)
        }
        GitHeadObservation::Unborn { .. } | GitHeadObservation::MissingTarget { .. } => None,
    }
}

/// Whether `refs/heads/<branch>` carries exactly one reflog entry that marks
/// it as created from the current checkout — the `git switch -c` evidence
/// behind §7.5 rename semantics. Missing reflogs never trigger a rename.
fn branch_is_newly_created(git: &GitRepository, branch: &str) -> bool {
    let Ok(reflog) = git.reflog(&format!("refs/heads/{branch}")) else {
        return false;
    };
    if reflog.len() != 1 {
        return false;
    }
    reflog
        .iter()
        .next()
        .and_then(|entry| entry.message().map(str::to_string))
        .is_some_and(|message| {
            let message = message.trim();
            message.starts_with("branch: Created from")
                || message.starts_with("branch: Created")
                || message.starts_with("Created from")
        })
}

fn pristine_map(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

fn observation_map(error: ObservationError) -> RepositoryError {
    RepositoryError::InvalidRepository {
        reason: error.to_string(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Atomic Tests")
            .env("GIT_AUTHOR_EMAIL", "tests@atomic.dev")
            .env("GIT_COMMITTER_NAME", "Atomic Tests")
            .env("GIT_COMMITTER_EMAIL", "tests@atomic.dev")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Colocated fixture: an Atomic repository over a Git repository with one
    /// commit on `dev`.
    fn colocated() -> (tempfile::TempDir, Repository, String, String) {
        let directory = tempfile::TempDir::new().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        git(directory.path(), &["init", "-b", "dev"]);
        git(
            directory.path(),
            &["config", "user.email", "tests@atomic.dev"],
        );
        git(directory.path(), &["config", "user.name", "Atomic Tests"]);
        fs::write(directory.path().join("tracked.txt"), b"tracked\n").unwrap();
        git(directory.path(), &["add", "tracked.txt"]);
        git(directory.path(), &["commit", "-m", "initial"]);
        let head = git(directory.path(), &["rev-parse", "HEAD"]);
        let tree = git(directory.path(), &["rev-parse", "HEAD^{tree}"]);
        (directory, repo, head, tree)
    }

    fn working_copy_id(repo: &Repository) -> WorkingCopyId {
        repo.require_working_copy_id().unwrap()
    }

    #[test]
    fn head_map_roundtrips_and_fails_closed_on_corruption() {
        let root = tempfile::TempDir::new().unwrap();
        let mut mappings = BTreeMap::new();
        mappings.insert(
            "a".repeat(40),
            HeadMapEntry {
                view: "git/aaaaaaa".to_string(),
                ephemeral: true,
            },
        );
        write_head_map(root.path(), &mappings).unwrap();
        assert_eq!(read_head_map(root.path()).unwrap(), mappings);

        fs::write(
            root.path().join(HEAD_MAP_RELATIVE),
            b"{\"version\":2,\"mappings\":{}}",
        )
        .unwrap();
        assert!(
            read_head_map(root.path()).is_err(),
            "future versions fail closed"
        );

        fs::write(root.path().join(HEAD_MAP_RELATIVE), b"not json").unwrap();
        assert!(read_head_map(root.path()).is_err());

        assert!(
            read_head_map(root.path().join("nowhere").as_path())
                .unwrap()
                .is_empty(),
            "a missing map reads as empty"
        );
    }

    #[test]
    fn rename_view_transaction_preserves_identity_repoints_records_and_drops_the_old_name() {
        let (directory, mut repo, _head, _tree) = colocated();
        let working_copy = working_copy_id(&repo);
        repo.create_view_with_identity("git/abc1234", ViewScope::Draft, Some("dev"))
            .unwrap();
        repo.update_working_copy_desired_view(working_copy, "git/abc1234")
            .unwrap();
        // Put a real change in the ephemeral view so identity is observable.
        fs::write(directory.path().join("tracked.txt"), b"ephemeral edit\n").unwrap();
        repo.add(working_copy, "tracked.txt", Default::default())
            .unwrap();
        repo.record(
            working_copy,
            atomic_core::change::ChangeHeader::new("ephemeral work"),
            crate::RecordOptions::new().add_path("tracked.txt"),
        )
        .unwrap();
        let old_info = repo.get_view_info("git/abc1234").unwrap();

        repo.rename_view_transaction("git/abc1234", "topic")
            .unwrap();

        let renamed = repo.get_view_info("topic").unwrap();
        assert_eq!(renamed.state, old_info.state, "Merkle identity preserved");
        assert_eq!(renamed.change_count, old_info.change_count);
        assert_eq!(renamed.scope, ViewScope::Draft);
        assert_eq!(renamed.parent_name.as_deref(), Some("dev"));
        assert!(matches!(
            repo.get_view_info("git/abc1234"),
            Err(RepositoryError::ViewNotFound { .. })
        ));
        let record = repo.working_copy_record(working_copy).unwrap();
        let txn = repo.pristine.read_txn().unwrap();
        let view = txn.get_view_by_id(record.desired_view).unwrap().unwrap();
        assert_eq!(view.name, "topic", "working copies are repointed");
        assert_eq!(record.desired_state, old_info.state);
    }

    #[test]
    fn rename_refuses_shared_views_existing_names_and_children() {
        let (_directory, mut repo, _head, _tree) = colocated();
        repo.create_view_with_identity("topic", ViewScope::Shared, None)
            .unwrap();
        // Shared views never rename through adoption.
        assert!(repo.rename_view_transaction("dev", "topic").is_err());
        // An existing target name is refused, never merged.
        repo.create_view_with_identity("eph", ViewScope::Draft, Some("dev"))
            .unwrap();
        assert!(repo.rename_view_transaction("eph", "topic").is_err());
        // A view with children refuses: reconciliation is CB-10A.
        repo.create_view_with_identity("parent-eph", ViewScope::Draft, Some("dev"))
            .unwrap();
        repo.create_view_with_identity("child", ViewScope::Draft, Some("parent-eph"))
            .unwrap();
        assert!(repo
            .rename_view_transaction("parent-eph", "renamed")
            .is_err());
    }

    #[test]
    fn ephemeral_names_extend_the_prefix_on_collision_and_reuse_the_same_commit() {
        let (directory, mut repo, head, _tree) = colocated();
        let head = head.clone();
        // A different commit's ephemeral view owns the 7-character prefix of
        // `other`, so the name for `head` extends one character.
        repo.create_view_with_identity("git/deadbee", ViewScope::Draft, None)
            .unwrap();
        let other = format!("deadbee{}", "0".repeat(33));
        assert_eq!(
            repo.ephemeral_view_name(&other).unwrap(),
            "git/deadbee0",
            "prefix extends on collision"
        );

        // A view named exactly after this commit's candidate is reused when
        // the head map ties it to the same commit.
        let candidate = format!("git/{}", &head[..7]);
        repo.create_view_with_identity(&candidate, ViewScope::Draft, None)
            .unwrap();
        write_head_map(
            directory.path(),
            &BTreeMap::from([(
                head.clone(),
                HeadMapEntry {
                    view: candidate.clone(),
                    ephemeral: true,
                },
            )]),
        )
        .unwrap();
        assert_eq!(repo.ephemeral_view_name(&head).unwrap(), candidate);
    }

    #[test]
    fn verified_checkpoint_records_detached_heads_without_a_symbol() {
        let (directory, repo, head, tree) = colocated();
        write_workspace_checkpoint(
            directory.path(),
            &WorkspaceCheckpoint {
                version: 2,
                view: "dev".to_string(),
                atomic_state: repo.get_view_info("dev").unwrap().state.to_string(),
                git_head_symref: None,
                git_head: head.clone(),
                git_tree: tree.clone(),
                git_index_tree: None,
                git_index_digest: None,
            },
        )
        .unwrap();
        let written = read_workspace_checkpoint(directory.path())
            .unwrap()
            .unwrap();
        assert_eq!(written.git_head_symref, None);

        // Re-publishing the verified checkpoint from an attached HEAD records
        // the branch symbol again.
        let checkout = git2::Repository::open(directory.path()).unwrap();
        let checkpoint = repo
            .write_verified_checkpoint(&checkout, "dev", &repo.get_view_info("dev").unwrap().state)
            .unwrap();
        assert_eq!(
            checkpoint.git_head_symref.as_deref(),
            Some("refs/heads/dev")
        );
        assert_eq!(checkpoint.git_head, head);
        assert_eq!(checkpoint.git_tree, tree);
    }

    #[test]
    fn branch_is_newly_created_distinguishes_switch_c_from_checkout() {
        let directory = tempfile::TempDir::new().unwrap();
        git(directory.path(), &["init", "-q", "-b", "main"]);
        git(
            directory.path(),
            &["config", "user.email", "tests@atomic.dev"],
        );
        git(directory.path(), &["config", "user.name", "Atomic Tests"]);
        fs::write(directory.path().join("f.txt"), b"one\n").unwrap();
        git(directory.path(), &["add", "f.txt"]);
        git(directory.path(), &["commit", "-m", "initial"]);
        // An existing branch with its own commit history (reflog has commits).
        git(directory.path(), &["checkout", "-qb", "old-branch"]);
        fs::write(directory.path().join("f.txt"), b"other\n").unwrap();
        git(directory.path(), &["add", "f.txt"]);
        git(directory.path(), &["commit", "-m", "second"]);
        git(directory.path(), &["checkout", "-q", "main"]);
        // `git switch -c` creates exactly one reflog entry.
        git(directory.path(), &["switch", "-qc", "fresh"]);

        let git_repo = git2::Repository::open(directory.path()).unwrap();
        assert!(branch_is_newly_created(&git_repo, "fresh"));
        assert!(!branch_is_newly_created(&git_repo, "old-branch"));
        assert!(!branch_is_newly_created(&git_repo, "never-existed"));
    }
}
