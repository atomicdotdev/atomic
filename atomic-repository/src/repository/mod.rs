//! Repository management for Atomic VCS
//!
//! This module provides the main `Repository` abstraction that coordinates
//! all VCS operations including initialization, opening existing repositories,
//! recording changes, and working copy management.
//!
//! # Views
//!
//! Atomic uses **Views** to organize changes. This is a fundamental conceptual
//! difference from Git:
//!
//! | Concept | Git Branches | Atomic Views |
//! |---------|--------------|--------------|
//! | Nature | Fork of history | Perspective on the graph |
//! | Data | Duplicates commits | References same changes |
//! | Merge | Combines divergent histories | Inserts missing changes |
//! | Identity | Pointer to a commit | Ordered sequence + Merkle state |
//!
//! Views are **perspectives** on the graph - they represent which changes have
//! been inserted and in what order. Multiple views can coexist, each showing a
//! different perspective on the same underlying data.
//!
//! # Change Storage
//!
//! Changes are stored in a content-addressed manner under `.atomic/changes/`.
//! The repository provides convenient methods for saving and loading changes
//! that integrate with the underlying [`ChangeStore`].
//!
//! ```rust,ignore
//! // Save a change
//! let hash = repo.save_change(&change)?;
//!
//! // Load it back
//! let loaded = repo.load_change(&hash)?;
//!
//! // Check if it exists
//! assert!(repo.has_change(&hash));
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomic_core::change::{Change, ChangeHeader, GraphOp};
pub use atomic_core::output::repo::MaterializedEntry;
use atomic_core::output::repo::{MaterializeResult, OutputItem};

use atomic_core::pristine::{
    GraphTxnT, GraphVisibilityClosure, MutTxnT, Pristine, TreeTxnT, ViewMembershipSet, ViewScope,
    ViewTxnT, WorkingCopyTxnT, CHANGE_FORMAT_VNEXT_CAPABILITY,
};
use atomic_core::record::workflow::retrieve::{RetrieveContentOptions, RetrieveResult};
use atomic_core::types::{Base32, Hash, Inode, Merkle, NodeId, Position, WorkingCopyId};

use crate::archive::{
    Archive, ArchiveEntry, ArchiveManifest, ArchiveOptions, ArchiveOutcome, DirectoryArchive,
};
use crate::changestore::{ChangeStore, ChangeStoreError, DEFAULT_CACHE_CAPACITY};
use crate::history::{HistoryOptions, HistorySummary};
use crate::ignore::IgnoreRules;
use crate::record::{
    build_header, filter_files, RecordError, RecordOptions, RecordOutcome, RecordStats,
};

use crate::remote::{RemoteConfig, RemoteEntry};
use crate::status::{
    collect_working_copy_files_with_rules, hash_file_contents, FileStatus, FileStatusEntry,
    RepositoryStatus, StatusOptions,
};

use crate::tracking::{
    add_to_tree, collect_files_for_tracking_with_rules, get_inode, is_tracked, list_tracked,
    move_tracked, normalize_path, normalize_path_with_root, remove_from_tree,
    should_ignore_with_rules, tracked_under_prefix, TrackedFile, TrackingError, TrackingOptions,
    TrackingStats,
};
use crate::unrecord::{UnrecordOptions, UnrecordOutcome};
use crate::RepositoryError;

// ── Sub-modules (new) ───────────────────────────────────────────────────

mod anchor;
mod adoption;
mod conflict_object;
mod conflict_reconcile;
mod cutover;
mod deferred_tree;
mod equivalence;
mod file_index_v2;
mod filter;
mod ignore_mirror;
mod git_observation;
mod binding_fetch;
mod binding_store;
mod locks;
mod materialize;
mod migration;
mod name_resolution;
pub mod observability;
mod operation;
mod project_tree;
mod projection;
mod projection_commit;
mod projection_effects;
pub mod ref_mapping;
mod repair;
mod sandbox;
mod semantic_materialize;
mod set_id;
mod snapshot;
mod snapshot_split;
mod split;
mod staging;
mod synthesis;
mod resurrection;
mod switch;
mod tag_projection;
mod views;
mod working_copy;
mod working_copy_reconcile;
mod workspace_txn;

// Re-export public items so external callers and sibling sub-modules that
// use `use super::*;` continue to resolve them at `crate::repository::…`.
pub use workspace_txn::{
    git_state_quiescent, git_locks_present, GitQuiescence, ReconcileEffectBudget,
    MIN_REACTIVE_QUIESCENCE_MS,
};
pub use cutover::{
    BridgeCutoverAudit, BridgeCutoverOutcome, BridgeCutoverPlan, CutoverAuditDisposition,
    CutoverSchemaAudit,
};
pub use anchor::{AdoptBoundHead, DetachedImportTarget};
pub use equivalence::{
    compare_project_state, compare_project_to_index, compare_project_to_worktree,
    verify_prospective_equivalence, EquivalenceClaims, EquivalenceLayer, EquivalenceMismatch,
    EquivalenceReport, MismatchKind, VerifiedProspectiveEquivalence,
};
pub use file_index_v2::FileIndexV2BackfillOutcome;
pub use filter::{
    collect_view_change_ids, collect_visible_change_ids, collect_visible_change_ids_with_deps,
    graph_visibility_closure, graph_visibility_from_membership, view_membership,
    view_membership_at_sequence,
};
pub use ignore_mirror::{
    check_ignore_policy, mirror_ignores, IgnoreMirrorError, IgnorePolicyReport,
    MANAGED_BLOCK_BEGIN, MANAGED_BLOCK_END,
};
pub use git_observation::{
    observe_colocated_git_readiness, observe_git_index, observe_git_metadata, observe_worktree,
    ATOMIC_DISPATCHER_MARKER, ATOMIC_LEGACY_MARKER_BEGIN, ColocatedGitForm,
    ColocatedGitReadiness, GitAdminEntryKind, GitAdminPathObservation, GitHeadObservation,
    GitObservationToken, GitOperationMarker, GitOperationMarkerObservation, GitOperationObservation,
    ObservationError, OwnedHookDispatcher, WorkspaceGitObservation,
    WorkspaceGitRepositoryObservation,
};
pub use locks::RepositoryCommonLockGuard;
pub use working_copy::detect_repository_root;
pub use working_copy::canonical_dot_dir_for;
pub use operation::{
    OperationDetails, OperationHeadState, OperationLog, OperationLogEntry,
    OperationVerificationState, PreparedBridgeGitWrite, PreparedRemoteOperation,
};
pub use project_tree::{
    escape_repo_path, unescape_repo_path, ConversionPolicy, ExclusionPolicy, ExclusionReason,
    GitIndexEntry, GitIndexState, GitObject, GitObjectDatabase, GitObjectKind, GitTree,
    GitTreeEntry, LossPolicy, ManifestDisposition, ManifestRoot, PhysicalKind,
    PlatformCapabilities, ProjectTree, ProjectTreeError, RepoPath, RepositoryEntry,
    RepositoryManifest, WorktreeEntry, WorktreeObservation, CONVERSION_POLICY_VERSION,
    GIT_INDEX_STATE_VERSION, REPOSITORY_MANIFEST_VERSION, REPO_PATH_VERSION,
    WORKTREE_OBSERVATION_VERSION,
};
pub use projection::effective_projection_closure;
pub use projection_commit::{
    armor_commit_signature, build_projected_commit, foreign_git_did, projection_commit_message,
    signature_header_value, strip_signature_header, unarmor_commit_signature,
    verify_commit_signature, write_projected_commit, write_raw_git_object, ProjectionAuthor,
    ProjectionCommitError, ProjectionCommitInput, ProjectionIdentityMap, ProjectionParents,
    ProjectionSigning, WholeViewMergeProof, ATOMIC_SIGNATURE_BEGIN, ATOMIC_SIGNATURE_END,
    COMMIT_SIGNATURE_DOMAIN,
};
pub use sandbox::{SealOptions, SealResult, StageOptions, StageResult, SANDBOX_POINTER};
pub use set_id::{effective_projection_identity, view_set_id, ViewIdentity};
pub use snapshot::{
    SnapshotRetentionOutcome, SnapshotRetentionPolicy, SnapshotState, SnapshotStatus,
};
pub use snapshot_split::{
    IndexEntryState, IndexManifest, IndexManifestEntry, SnapshotSplitRefusal, SplitSnapshotError,
    SplitSnapshotOutcome, INDEX_MANIFEST_VERSION,
};
pub use staging::{
    format_two_column, git_object_id_hex, observe_git_staging_state, observe_staging_state,
    quote_path, BaselineEntry, StageCode, StagingEntry, StagingError, StagingNotice, StagingState,
    ASSUME_VALID_FLAG, INTENT_TO_ADD_FLAG, SKIP_WORKTREE_FLAG, STAGING_STATE_VERSION,
};
pub use split::{SplitChange, SplitOptions, SplitOutcome};
pub use projection_effects::{PreparedProjectionPublish, ProjectionCheckpointPlan};
pub use tag_projection::TagProjectionOutcome;
pub use conflict_reconcile::{
    ConflictReconcileOutcome, StaleConflictDisposition, StaleConflictPath, StaleConflictReport,
};
pub use views::{ManifestApplyOutcome, ViewInfo};
pub use working_copy_reconcile::{
    WorkingCopyReconcileOutcome, WorkingCopyRegistrationDiagnosis,
};
pub use workspace_txn::{
    GitOperationDisposition, UnanchoredWorkspace, WorkspaceCheckpoint, WorkspaceEntryPlan,
    WorkspaceFilesystemPlan, WorkspaceHeadPlan, WorkspaceRefPlan, WorkspaceRemediation,
    WorkspaceTxn, WorkspaceTxnMode, WorkspaceTxnStart, MAX_WORKSPACE_ENTRY_PLAN_ITEMS,
    MAX_WORKSPACE_TXN_ATTEMPTS,
};
// Re-import workspace helpers from `switch` so they are available to
// `mod.rs` (used in `init`) and to sibling sub-modules via `use super::*;`.
use switch::{ensure_workspace_dir, workspace_path};

// ── Sub-modules (existing) ──────────────────────────────────────────────

mod archive;
mod attributes;
mod changes;
mod content;
mod history;
mod insert;
pub mod provenance_gate;
mod provenance_summary;
mod record;
mod remotes;
mod status;
mod tags;
mod tracking;
mod triage;
mod vault;
mod vault_defaults;
mod vault_embeddings;
mod vault_goal;
mod vault_identity;
mod vault_intent;
mod vault_kg_enrich;
mod vault_names;
mod vault_triples;
mod verify;
pub use insert::{
    ImportLineIndexSeed, ImportLineIndexSeedLine, ImportWriteOutcome, ImportWriteTimings,
};
pub use synthesis::{
    git_resolution_metadata, git_synthesis_metadata, GitResolutionOrigin, GitSynthesisOrigin,
    ResurrectionOutcome, StagedExpectation, StagedPathExpectation, StagedTreeExpectation,
    SynthesisOutcome,
};
pub use resurrection::{
    ExactResurrection, GitShaResolution, ProjectionProof,
};
pub use anchor::{
    conversion_policy_for_git, format_equivalence_report, read_head_map,
    BridgeAnchorError, BridgeAnchorOutcome, BridgeAnchorRefusal, HeadMapEntry,
};
pub use provenance_summary::ProvenanceSummary;
pub use provenance_gate::{
    GateBlocker, GateVerdict, MacKeyProvider, PublicationGateConfig,
};
pub use repair::{
    NativeIndex, NativeIndexProblem, NativeIndexProblemKind, NativeIndexRepairOutcome,
    NativeIndexReport,
};
pub use semantic_materialize::{CrdtMaterializeOptions, CrdtMaterializeOutcome};
pub use tags::{deserialize_tag, serialize_tag};
pub use triage::{BaggageEntry, CandidateSet, Coverage};
pub use vault_embeddings::{hash_embed, EmbedConfig, TextChunk};
pub use vault_goal::{
    GoalInfo, GoalStartOptions, GoalStartResult, GoalStopOptions, GoalStopResult,
};
pub use vault_identity::VaultIdentity;
pub use vault_intent::{
    IntentCreateOptions, IntentCreateResult, IntentDeleteResult, IntentInfo, IntentUpdateOptions,
};
pub use vault_kg_enrich::KgEnrichStats;
pub use vault_names::{derive_intent_prefix, generate_goal_name};
pub use binding_fetch::{
    BindingChangeSource, ClosureAcquisition, ClosureReadiness, IncompletenessReason,
};
pub use binding_store::{BindingPublication, VerifiedPublishedBinding, BINDING_REF_PREFIX};
pub use conflict_object::{
    ConflictClaimantObject, ConflictEntryKind, ConflictEntryObject, ConflictFileObject,
    ConflictObjectError, ConflictRepresentability, ConflictSetObject, ConflictSideObject,
    CONFLICT_SET_MAGIC, CONFLICT_SET_VERSION,
};
pub use verify::{VerifyProblem, VerifyReport};

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::{create_temp_repo, create_test_change};

// ── Constants ───────────────────────────────────────────────────────────

/// The name of the Atomic directory
pub const DOT_DIR: &str = ".atomic";

/// The default view name
pub const DEFAULT_VIEW: &str = "dev";

/// Backward-compatible alias for [`DEFAULT_VIEW`].
pub const DEFAULT_STACK: &str = DEFAULT_VIEW;

/// Canonical redb change-store filename inside [`.atomic`](DOT_DIR).
///
/// The filesystem-backed [`ChangeStore`] remains authoritative during the
/// additive migration. This database is the single repository-local location
/// for redb-native changes and provenance and is opened by the repository owner
/// service, not by ordinary `Repository` handles.
pub const REDB_CHANGE_STORE_FILE: &str = "changes.redb";

/// Subdirectory inside `.atomic/` that holds per-view workspace state.
///
/// Each view gets a directory at `.atomic/workspaces/<view_name>/` where
/// ignored/artifact files are shelved on `switch_view` and restored when
/// switching back.  This is the mechanism by which views achieve full
/// working copy isolation — not just tracked files (managed by the graph)
/// but also build artifacts like `node_modules/`, `dist/`, `.next/`, etc.
const WORKSPACES_DIR: &str = "workspaces";

// ── Repository struct ───────────────────────────────────────────────────

/// An Atomic repository.
///
/// The Repository struct is the main entry point for all VCS operations.
/// It manages the repository's pristine (database), changes directory,
/// working copy, and configuration.
///
/// # Components
///
/// - **Pristine**: The graph database storing all version control data
/// - **ChangeStore**: Content-addressed storage for change files
/// - **Working Copy**: The actual files in the repository (future)
///
/// # Thread Safety
///
/// The `Repository` struct is `!Sync` due to the internal caching in
/// [`ChangeStore`]. For concurrent access, use separate `Repository`
/// instances or wrap in appropriate synchronization primitives.
pub struct Repository {
    /// Root path of the repository (contains .atomic/)
    root: PathBuf,
    /// Path to the .atomic directory
    dot_dir: PathBuf,
    /// Current view name
    current_view: String,
    /// The pristine database handle.
    ///
    /// Wrapped in `Arc` so that multiple `Repository` instances _can_
    /// share the same underlying redb `Database` and avoid stale-snapshot
    /// bugs. In practice, sharing only happens when constructing via
    /// `open_with_pristine`; `open` / `open_readonly` create a fresh
    /// `Pristine` for each `Repository`.
    pristine: Arc<Pristine>,
    /// The filesystem change store for persisting canonical `.change` files.
    change_store: ChangeStore,
    /// Whether this handle was opened for an agent sandbox (via
    /// [`Repository::open_sandbox`]). A sandbox's `dot_dir`/`pristine`/
    /// `change_store` point at the canonical repository while `current_view`
    /// holds the sandbox's view (from its pointer file); it must not repoint
    /// the canonical `current_view` on disk.
    is_sandbox: bool,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repository")
            .field("root", &self.root)
            .field("dot_dir", &self.dot_dir)
            .field("current_view", &self.current_view)
            .field("pristine", &"<Pristine>")
            .field("change_store", &self.change_store)
            .field("is_sandbox", &self.is_sandbox)
            .finish()
    }
}

// ── Construction, accessors, internal helpers ────────────────────────────

impl Repository {
    /// Initialize a new repository at the given path.
    ///
    /// This creates the `.atomic` directory structure and initializes
    /// the database with an empty graph.
    ///
    /// # Arguments
    ///
    /// * `path` - The directory to initialize as a repository
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A repository already exists at the path
    /// - The directory cannot be created
    /// - The database cannot be initialized
    pub fn init<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        Self::init_with_view(path, DEFAULT_VIEW)
    }

    /// Initialize a new repository whose default view has the given name.
    ///
    /// Behaves like [`Repository::init`], but the initial shared root view —
    /// and the config's `[view] default` — is `view_name` instead of
    /// [`DEFAULT_VIEW`]. This is used by `atomic git import` so the imported
    /// repository's default view matches the Git default branch rather than
    /// leaving an unused `dev` view behind.
    ///
    /// # Arguments
    ///
    /// * `path` - The directory to initialize as a repository
    /// * `view_name` - The name of the initial shared root view
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A repository already exists at the path
    /// - The directory cannot be created
    /// - The database cannot be initialized
    pub fn init_with_view<P: AsRef<Path>>(
        path: P,
        view_name: &str,
    ) -> Result<Self, RepositoryError> {
        let root = path.as_ref().to_path_buf();
        let dot_dir = root.join(DOT_DIR);

        // Check if repository already exists
        if dot_dir.exists() {
            return Err(RepositoryError::AlreadyExists {
                path: root.display().to_string(),
            });
        }

        // Create directory structure
        std::fs::create_dir_all(&dot_dir)?;
        std::fs::create_dir_all(dot_dir.join("changes"))?;
        std::fs::create_dir_all(dot_dir.join(WORKSPACES_DIR))?;

        // Create initial config
        let config_path = dot_dir.join("config.toml");
        let initial_config = format!(
            r#"# Atomic repository configuration

[view]
default = "{}"
"#,
            view_name
        );
        std::fs::write(&config_path, initial_config)?;

        // Initialize the pristine database (redb creates the file)
        let pristine =
            Arc::new(Pristine::open(dot_dir.join("pristine.redb")).map_err(RepositoryError::from)?);

        // Create the default view and its workspace directory
        {
            let mut txn = pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.open_or_create_view(view_name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        ensure_workspace_dir(&dot_dir, view_name)?;

        // Persist the working-copy record before publishing compatibility files.
        let layout = working_copy::discover_layout(&root)?;
        let (_working_copy_id, current_view) =
            working_copy::migrate_identity(&pristine, &layout, view_name)?;

        // Initialize the change store
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let repository = Self {
            root,
            dot_dir,
            current_view,
            pristine,
            change_store,
            is_sandbox: false,
        };

        Ok(repository)
    }

    /// Open an existing repository.
    ///
    /// This searches for a `.atomic` directory starting from the given path
    /// and walking up to parent directories.
    ///
    /// # Arguments
    ///
    /// * `path` - A path inside the repository (or the repository root)
    ///
    /// # Errors
    ///
    /// Returns an error if no repository is found.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        Self::open_with_budget(path, ReconcileEffectBudget::Command)
    }

    /// Open an existing repository under an explicit effect budget (CB-13D).
    ///
    /// Under [`ReconcileEffectBudget::MetadataOnly`] the writable open
    /// refuses *before* any recovery runs when the repository holds pending
    /// work whose recovery would execute effect-bearing plans (deferred
    /// tree alignment, incomplete working-copy or repository operation
    /// heads): the returned error is
    /// [`RepositoryError::ReactiveDeferred`] and nothing was mutated. A
    /// [`ReconcileEffectBudget::Command`] open behaves exactly like
    /// [`open`](Self::open).
    pub fn open_with_budget<P: AsRef<Path>>(
        path: P,
        budget: ReconcileEffectBudget,
    ) -> Result<Self, RepositoryError> {
        if let Some((working_root, canonical, view)) = sandbox::detect_sandbox(path.as_ref()) {
            // CB-13D ::24 R1: the sandbox open passes the SAME budget gate
            // as the ordinary open — it must not return before the
            // recovery fence (a metadata-only sandbox open refuses unsafe
            // pending recovery instead of replaying it).
            let mut sandbox = Self::open_sandbox(working_root, canonical, &view)?;
            let working_copy = sandbox.require_working_copy_id()?;
            sandbox.fence_recovery_for_budget(working_copy, budget)?;
            return Ok(sandbox);
        }

        let mut layout = working_copy::discover_layout(path.as_ref())?;
        let root = layout.working_root.clone();
        let dot_dir = layout.common_dot_dir.clone();

        // PATH_CLAIMS migration still needs a view name. Prefer an existing
        // working-copy record; consult legacy compatibility files only when no
        // record owns this canonical location yet.
        let legacy_view =
            Self::read_legacy_current_view(&layout.working_copy_dot_dir, &layout.common_dot_dir)?;

        // Capability validation happens inside Pristine::open before additive
        // table initialization commits. No linked-worktree pointer, change-store
        // directory, migration, or recovery mutation may precede this open.
        let pristine_path = dot_dir.join("pristine.redb");
        let pristine = Pristine::open(&pristine_path).map_err(RepositoryError::from)?;

        working_copy::ensure_repository_pointer(&layout)?;
        layout.pointer_needs_write = false;
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let migration_view =
            working_copy::registered_view_name(&pristine, &layout)?.unwrap_or(legacy_view);
        let pristine = migration::migrate_path_claims_if_required(
            &pristine_path,
            pristine,
            &change_store,
            &migration_view,
        )?;
        let (working_copy_id, current_view) =
            working_copy::migrate_identity(&pristine, &layout, &migration_view)?;
        let pristine = Arc::new(pristine);

        let mut repository = Self {
            root,
            dot_dir,
            current_view,
            pristine,
            change_store,
            is_sandbox: false,
        };
        repository.fence_recovery_for_budget(working_copy_id, budget)?;

        // Deferred TREE or operation recovery may rewrite the compatibility
        // pointer. Reassert the persistent record as the sole authority.
        let (_id, authoritative_view) =
            working_copy::load_registered_identity(&repository.pristine, &layout)?;
        repository.current_view = authoritative_view.clone();
        working_copy::write_current_view_compatibility(
            &layout.working_copy_dot_dir,
            &authoritative_view,
        )?;
        Ok(repository)
    }

    /// Open an existing repository without the table-init write lock.
    ///
    /// Like [`open`](Self::open) but uses [`Pristine::open_existing`] which
    /// skips the `begin_write()` table-initialization transaction.  The
    /// returned repository still supports write operations — the write lock
    /// is deferred until `write_txn()` is actually called.
    ///
    /// Use this for short-lived processes (agent hooks, background jobs)
    /// where blocking on the init write lock would hang the process.
    /// The shared open-time recovery fence (CB-13D ::24 R1): a
    /// metadata-only budget refuses unsafe pending recovery instead of
    /// replaying it inside the open; a command budget runs the recovery
    /// steps. The ordinary open and the sandbox open pass this gate.
    pub(crate) fn fence_recovery_for_budget(
        &mut self,
        working_copy: WorkingCopyId,
        budget: ReconcileEffectBudget,
    ) -> Result<(), RepositoryError> {
        let operation_lock = self.try_lock_operation(working_copy)?;
        // CB-13D review R1: a metadata-only open fences recovery — refuse
        // before any effect-bearing recovery step instead of replaying it
        // inside the open.
        if budget.is_metadata_only() {
            if let Some(detail) = self.pending_unsafe_recovery_work(working_copy)? {
                drop(operation_lock);
                return Err(RepositoryError::ReactiveDeferred { detail });
            }
        }
        self.recover_pending_deferred_tree_alignment_locked(&operation_lock)?;
        self.ensure_repository_operation_safe_for(&operation_lock)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        self.recover_incomplete_operation(&operation_lock)?;
        drop(operation_lock);
        Ok(())
    }

    pub fn open_existing<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        if let Some((working_root, canonical, view)) = sandbox::detect_sandbox(path.as_ref()) {
            // CB-13D ::24 R1: the sandbox open runs the SAME recovery
            // steps as the ordinary open_existing (which recovers
            // unconditionally) — the sandbox path must not return before
            // that gate.
            let mut sandbox = Self::open_sandbox(working_root, canonical, &view)?;
            let working_copy = sandbox.require_working_copy_id()?;
            sandbox.fence_recovery_for_budget(working_copy, ReconcileEffectBudget::Command)?;
            return Ok(sandbox);
        }
        let layout = working_copy::discover_layout(path.as_ref())?;
        let root = layout.working_root.clone();
        let dot_dir = layout.common_dot_dir.clone();

        let pristine = Arc::new(
            Pristine::open_existing(dot_dir.join("pristine.redb"))
                .map_err(RepositoryError::from)?,
        );
        let (working_copy_id, current_view) =
            working_copy::load_registered_identity(&pristine, &layout)?;

        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut repository = Self {
            root,
            dot_dir,
            current_view,
            pristine,
            change_store,
            is_sandbox: false,
        };
        let operation_lock = repository.try_lock_operation(working_copy_id)?;
        repository.recover_pending_deferred_tree_alignment_locked(&operation_lock)?;
        repository.ensure_repository_operation_safe_for(&operation_lock)?;
        if let OperationHeadState::Diverged(heads) =
            repository.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy_id)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        repository.recover_incomplete_operation(&operation_lock)?;
        drop(operation_lock);
        let (_id, authoritative_view) =
            working_copy::load_registered_identity(&repository.pristine, &layout)?;
        repository.current_view = authoritative_view;
        Ok(repository)
    }

    /// Open an existing repository for a workspace transaction preflight.
    ///
    /// This constructor supports later writes but performs no migration, operation
    /// recovery, head consolidation, compatibility-file rewrite, or table
    /// initialization. Callers must immediately enter [`Self::begin_workspace_txn`],
    /// which observes and classifies Git state before any recovery mutation.
    pub fn open_for_workspace_transaction<P: AsRef<Path>>(
        path: P,
    ) -> Result<Self, RepositoryError> {
        if let Some((working_root, canonical, view)) = sandbox::detect_sandbox(path.as_ref()) {
            // CB-13D ::24 R1: this constructor performs NO recovery on
            // either path (the ordinary path defers everything to
            // begin_workspace_txn), so the sandbox path matches it and
            // returns without a recovery gate by design.
            return Self::open_sandbox(working_root, canonical, &view);
        }
        let layout = working_copy::discover_layout(path.as_ref())?;
        let root = layout.working_root.clone();
        let dot_dir = layout.common_dot_dir.clone();
        let pristine = Arc::new(
            Pristine::open_existing(dot_dir.join("pristine.redb"))
                .map_err(RepositoryError::from)?,
        );
        let (_working_copy_id, current_view) =
            working_copy::load_registered_identity(&pristine, &layout)?;
        let change_store =
            ChangeStore::open_existing(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_view,
            pristine,
            change_store,
            is_sandbox: false,
        })
    }

    /// Open an existing repository in read-only mode.
    ///
    /// This method opens the repository without acquiring a write lock on the
    /// database, allowing concurrent read access from multiple processes. It's
    /// suitable for read-only operations like `status`, `diff`, `log`, and `change`.
    ///
    /// Use this method when you only need to query the repository state and don't
    /// need to make any modifications. This is especially useful for:
    /// - CLI commands that only display information
    /// - Integration tools that poll repository status
    /// - Concurrent readers of a database that has no writable process handle
    ///
    /// # Arguments
    ///
    /// * `path` - Any path within the repository
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository is found at or above the given path
    /// - The database file doesn't exist or is corrupted
    /// - Read access cannot be obtained
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Open for read-only status check
    /// let repo = Repository::open_readonly(".")?;
    /// let status = repo.status(StatusOptions::default())?;
    /// ```
    pub fn open_readonly<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        Self::open_readonly_mode(path.as_ref(), false)
    }

    /// Open read-only for operation-log diagnosis without performing recovery.
    ///
    /// Unlike [`Self::open_readonly`], this narrow mode permits incomplete or
    /// multi-head operation state so `atomic op log|show` can explain the fault.
    /// It never repairs deferred tree alignment or executes operation effects.
    pub fn open_readonly_for_operation_inspection<P: AsRef<Path>>(
        path: P,
    ) -> Result<Self, RepositoryError> {
        Self::open_readonly_mode(path.as_ref(), true)
    }

    fn open_readonly_mode(
        path: &Path,
        operation_inspection: bool,
    ) -> Result<Self, RepositoryError> {
        if let Some((working_root, canonical, view)) = sandbox::detect_sandbox(path) {
            return Self::open_sandbox_readonly(working_root, canonical, &view);
        }
        let layout = working_copy::discover_layout(path)?;
        let root = layout.working_root.clone();
        let dot_dir = layout.common_dot_dir.clone();

        // Open the pristine database in read-only mode. Identity validation below
        // performs no repair and reports a typed migration-required error.
        let pristine = Arc::new(
            Pristine::open_readonly(dot_dir.join("pristine.redb"))
                .map_err(RepositoryError::from)?,
        );
        let (working_copy_id, current_view) =
            working_copy::load_registered_identity(&pristine, &layout)?;

        // Open the change store without creating missing paths.
        let change_store =
            ChangeStore::open_existing(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let repository = Self {
            root,
            dot_dir,
            current_view,
            pristine,
            change_store,
            is_sandbox: false,
        };
        if !operation_inspection
            && (repository.has_pending_deferred_tree_alignment()
                || repository.working_copy_operation_requires_recovery(working_copy_id)?
                || repository.repository_operation_requires_recovery()?)
        {
            return Err(RepositoryError::InvalidOperation {
                message: "repository operation is still completing; retry with a writable repository open"
                    .to_string(),
            });
        }
        Ok(repository)
    }

    /// Open an existing repository for native-index verification without
    /// requiring derived-schema completion or performing any repair/migration.
    pub fn open_readonly_for_native_repair<P: AsRef<Path>>(
        path: P,
    ) -> Result<Self, RepositoryError> {
        Self::open_for_native_repair_mode(path.as_ref(), true)
    }

    /// Open an existing repository for one explicit native-index repair.
    ///
    /// This bypasses automatic migration and deferred alignment so every write
    /// remains inside the repair transaction itself.
    pub fn open_for_native_repair<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        Self::open_for_native_repair_mode(path.as_ref(), false)
    }

    fn open_for_native_repair_mode(path: &Path, readonly: bool) -> Result<Self, RepositoryError> {
        if sandbox::detect_sandbox(path).is_some() {
            return Err(RepositoryError::InvalidOperation {
                message: "native-index doctor must run from the canonical repository working copy"
                    .to_string(),
            });
        }
        let root = Self::find_root(path)?;
        let dot_dir = root.join(DOT_DIR);
        let pristine_path = dot_dir.join("pristine.redb");
        let pristine = if readonly {
            Pristine::open_readonly_for_repair(pristine_path)
        } else {
            Pristine::open_existing_for_repair(pristine_path)
        }
        .map_err(RepositoryError::from)?;
        let current_view = Self::read_current_view(&dot_dir)?;
        let change_store =
            ChangeStore::open_existing(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let repository = Self {
            root,
            dot_dir,
            current_view,
            pristine: Arc::new(pristine),
            change_store,
            is_sandbox: false,
        };
        if repository.has_pending_deferred_tree_alignment() {
            return Err(RepositoryError::InvalidOperation {
                message: "repository view switch is still completing; native-index doctor refuses implicit recovery"
                    .to_string(),
            });
        }
        Ok(repository)
    }

    /// Open for a short-lived writer, waiting at most `timeout` for an
    /// incompatible process handle to close. Other errors return immediately.
    /// The caller must not already hold a handle to the same database.
    pub fn open_existing_wait<P: AsRef<Path>>(
        path: P,
        timeout: std::time::Duration,
    ) -> Result<Self, RepositoryError> {
        Self::wait_for_database(timeout, || Self::open_existing(path.as_ref()))
    }

    /// Read-only counterpart of [`Self::open_existing_wait`].
    pub fn open_readonly_wait<P: AsRef<Path>>(
        path: P,
        timeout: std::time::Duration,
    ) -> Result<Self, RepositoryError> {
        Self::wait_for_database(timeout, || Self::open_readonly(path.as_ref()))
    }

    fn wait_for_database(
        timeout: std::time::Duration,
        mut open: impl FnMut() -> Result<Self, RepositoryError>,
    ) -> Result<Self, RepositoryError> {
        let start = std::time::Instant::now();
        loop {
            match open() {
                Err(RepositoryError::DatabaseBusy) if start.elapsed() < timeout => {
                    std::thread::sleep(
                        std::time::Duration::from_millis(10)
                            .min(timeout.saturating_sub(start.elapsed())),
                    );
                }
                result => return result,
            }
        }
    }

    /// Open for a workspace transaction, waiting for transient writers.
    ///
    /// Same semantics as [`Self::open_for_workspace_transaction`], but typed
    /// database contention (another process publishing a stop checkpoint or
    /// recording) is retried for up to `timeout` instead of failing fast.
    /// Callers such as `atomic status` and `atomic diff` reconcile at their
    /// own pace and must not surface transient publication as an error.
    pub fn open_for_workspace_transaction_wait<P: AsRef<Path>>(
        path: P,
        timeout: std::time::Duration,
    ) -> Result<Self, RepositoryError> {
        Self::wait_for_database(timeout, || {
            Self::open_for_workspace_transaction(path.as_ref())
        })
    }

    /// Open an existing repository using a pre-opened `Pristine`.
    ///
    /// This constructor is used by the storage server to share a single
    /// `Pristine` (redb `Database`) handle across concurrent requests to
    /// the same project.  Sharing the handle ensures every write transaction
    /// sees the committed state of prior transactions — opening a fresh
    /// `Database` per request can see stale snapshots.
    ///
    /// # Requirements
    ///
    /// The provided `pristine` **must** have been opened from
    /// `<path>/.atomic/pristine.redb` (i.e. the same repository that
    /// `path` resolves to). Passing a `Pristine` from a different
    /// repository will silently couple one repo's configuration and
    /// change store with another repo's database, leading to corruption.
    pub fn open_with_pristine<P: AsRef<Path>>(
        path: P,
        pristine: Arc<Pristine>,
    ) -> Result<Self, RepositoryError> {
        let mut layout = working_copy::discover_layout(path.as_ref())?;
        pristine
            .ensure_supported_repository_capabilities()
            .map_err(RepositoryError::from)?;
        working_copy::ensure_repository_pointer(&layout)?;
        layout.pointer_needs_write = false;
        let root = layout.working_root.clone();
        let dot_dir = layout.common_dot_dir.clone();
        let initial_view =
            Self::read_legacy_current_view(&layout.working_copy_dot_dir, &layout.common_dot_dir)?;
        let (working_copy_id, current_view) =
            working_copy::migrate_identity(&pristine, &layout, &initial_view)?;

        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut repository = Self {
            root,
            dot_dir,
            current_view,
            pristine,
            change_store,
            is_sandbox: false,
        };
        let operation_lock = repository.try_lock_operation(working_copy_id)?;
        repository.recover_pending_deferred_tree_alignment_locked(&operation_lock)?;
        repository.ensure_repository_operation_safe_for(&operation_lock)?;
        if let OperationHeadState::Diverged(heads) =
            repository.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy_id)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        repository.recover_incomplete_operation(&operation_lock)?;
        drop(operation_lock);
        let (_id, authoritative_view) =
            working_copy::load_registered_identity(&repository.pristine, &layout)?;
        repository.current_view = authoritative_view.clone();
        working_copy::write_current_view_compatibility(
            &layout.working_copy_dot_dir,
            &authoritative_view,
        )?;
        Ok(repository)
    }

    /// Find the repository root by searching for .atomic directory.
    ///
    /// Starts at the given path and walks up to parent directories until
    /// a `.atomic` directory is found that contains `pristine.redb` (indicating
    /// it's a repository, not just a config directory like `~/.atomic/`).
    ///
    /// The search stops at the user's home directory to prevent accidentally
    /// treating the entire home directory as a repository.
    pub fn find_root(start: &Path) -> Result<PathBuf, RepositoryError> {
        working_copy::discover_layout(start).map(|layout| layout.working_root)
    }

    // ── Path accessors ──────────────────────────────────────────────────

    /// Check if a path is inside an Atomic repository.
    pub fn is_repository<P: AsRef<Path>>(path: P) -> bool {
        Self::find_root(path.as_ref()).is_ok()
    }

    /// Resolve the **canonical** `.atomic` directory for a working path
    /// *without* opening the database.
    ///
    /// If `path` is inside an agent sandbox, follows the `.atomic-sandbox`
    /// pointer to the canonical repository; otherwise finds the enclosing
    /// repository root. Returns `<canonical_root>/.atomic`.
    ///
    /// Use this for lock-free filesystem access to the canonical graph — e.g.
    /// writing a provenance graph through [`ChangeStore`] —
    /// where opening a full [`Repository`] would take the redb lock. In a
    /// sandbox `path/.atomic` is a throwaway local dir (or absent), so joining
    /// `.atomic` onto the working root would miss the real graph entirely.
    pub fn canonical_dot_dir<P: AsRef<Path>>(path: P) -> Result<PathBuf, RepositoryError> {
        if let Some((_working, canonical, _view)) = sandbox::detect_sandbox(path.as_ref()) {
            return Ok(working_copy::discover_layout(&canonical)?.common_dot_dir);
        }
        Ok(working_copy::discover_layout(path.as_ref())?.common_dot_dir)
    }

    /// Resolve the canonical redb change-store path without opening redb.
    ///
    /// Sandboxes resolve to the owning repository's `.atomic/changes.redb`,
    /// never to a sandbox-local database.
    pub fn canonical_change_store_path<P: AsRef<Path>>(
        path: P,
    ) -> Result<PathBuf, RepositoryError> {
        Ok(Self::canonical_dot_dir(path)?.join(REDB_CHANGE_STORE_FILE))
    }

    /// Get the repository root path.
    #[inline]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the .atomic directory path.
    #[inline]
    pub fn dot_dir(&self) -> &Path {
        &self.dot_dir
    }

    /// Get the pristine (database) file path.
    #[inline]
    pub fn pristine_path(&self) -> PathBuf {
        self.dot_dir.join("pristine.redb")
    }

    /// Get the changes directory path.
    #[inline]
    pub fn changes_dir(&self) -> PathBuf {
        self.dot_dir.join("changes")
    }

    /// Get the canonical redb change-store path.
    #[inline]
    pub fn redb_change_store_path(&self) -> PathBuf {
        self.dot_dir.join(REDB_CHANGE_STORE_FILE)
    }

    /// Get the current view name.
    #[inline]
    pub fn current_view(&self) -> &str {
        &self.current_view
    }

    /// Whether this handle was opened for an agent sandbox.
    ///
    /// A sandbox records into the canonical graph on the view named in its
    /// pointer file (held in `current_view`, never written back to the
    /// canonical `current_view` on disk), so callers must not fork a new view
    /// or repoint the current view for it.
    #[inline]
    pub fn is_sandbox(&self) -> bool {
        self.is_sandbox
    }

    /// Get the config file path.
    #[inline]
    pub fn config_path(&self) -> PathBuf {
        self.dot_dir.join("config.toml")
    }

    // ── View pointer (internal state) ───────────────────────────────────

    /// Set the current view (internal, does not update working copy).
    ///
    /// Update the view pointer on disk without materializing.
    ///
    /// This updates both the in-memory state and persists the change to disk,
    /// but does **NOT** update the working copy.  The working copy may be
    /// left inconsistent with the view pointer — `status()` handles this
    /// gracefully (files tracked on other views show as `Added` rather than
    /// `Untracked`), but callers should prefer `switch_view()` for any
    /// user-facing view change.
    ///
    /// This is `pub(crate)` to prevent external code from accidentally
    /// desynchronising the view pointer and working copy.  Internal uses
    /// (init, git import, agent record alignment) know they will
    /// materialise or record immediately afterward.
    ///
    /// # Errors
    ///
    /// Returns an error if the view does not exist or the pointer file
    /// cannot be written.
    pub fn set_current_view(
        &mut self,
        working_copy: WorkingCopyId,
        view: &str,
    ) -> Result<(), RepositoryError> {
        // Verify the view exists in the pristine database
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            if txn
                .get_view(view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .is_none()
            {
                return Err(RepositoryError::ViewNotFound {
                    name: view.to_string(),
                });
            }
        }

        self.update_working_copy_desired_view(working_copy, view)?;
        self.write_current_view(view)?;
        self.current_view = view.to_string();
        Ok(())
    }

    /// Align the view pointer without materializing the working copy.
    ///
    /// This persists the new view name to `.atomic/current_view` so that
    /// subsequent `status()`, `add()`, and `record()` calls target the
    /// correct view, but it does **not** add, remove, or update any files
    /// on disk.
    ///
    /// Use this only when the caller will immediately populate the working
    /// copy itself (e.g. the agent record hook, which creates files and
    /// then records them).  For interactive view switches, use
    /// [`Repository::switch_view`] instead — it materializes the working copy and
    /// prevents desync between the pointer and disk state.
    ///
    /// `status()` handles the desynchronised state gracefully: files that
    /// are tracked globally (in TREE) but whose introducing change belongs
    /// to another view appear as `Added` (not `Untracked`), so there is
    /// no contradictory "Untracked + already tracked" state even if the
    /// caller forgets to materialise.
    ///
    /// # Errors
    ///
    /// Returns an error if the view does not exist or the pointer file
    /// cannot be written.
    pub fn align_to_view(
        &mut self,
        working_copy: WorkingCopyId,
        view: &str,
    ) -> Result<(), RepositoryError> {
        let operation_lock = self.try_lock_operation(working_copy)?;
        self.recover_pending_deferred_tree_alignment_locked(&operation_lock)?;
        let (record, target_view) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let record = txn
                .get_working_copy(working_copy)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
            let target_view = txn
                .get_view(view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;
            (record, target_view)
        };
        let mut target = operation::working_copy_state_ref(record);
        target.desired_view = target_view.id;
        target.desired_state = target_view.state;
        target.materialized_state = None;
        target.materialized_manifest = None;
        self.apply_working_copy_state_locked(&operation_lock, &target)
    }

    /// Set the current view on this handle only.
    ///
    /// This does not validate the view or persist `.atomic/current_view`. It is
    /// intended for scoped handles that must read or write another validated
    /// view without publishing a process-global working-copy switch (for
    /// example agent recording or background Git import).
    #[inline]
    pub fn set_current_view_in_memory(&mut self, view: &str) {
        self.current_view = view.to_string();
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Get a reference to the pristine database.
    #[inline]
    /// Whether `oid` is reachable from `expected_tip` in the colocated Git
    /// repository at `root` (CB-12B follow-up AC-4 exact-OID binding: pushed
    /// foreign history legitimately proposes ancestors of the verified
    /// projection; nothing outside that ancestry is bound by the gate).
    /// Read-only; the root must be a Git repository. `false` on any
    /// lookup failure — the caller refuses unbound OIDs.
    pub fn git_oid_is_ancestor(root: &std::path::Path, oid: &str, expected_tip: &str) -> bool {
        let Ok(git) = git2::Repository::open(root) else {
            return false;
        };
        let Ok(oid) = git2::Oid::from_str(oid) else {
            return false;
        };
        let Ok(tip) = git2::Oid::from_str(expected_tip) else {
            return false;
        };
        if oid == tip {
            return true;
        }
        // Walk ancestry from the tip: any reachable commit with the exact
        // OID proves the binding. Depth-bounded by the full walk (read-only).
        let Ok(commit) = git.find_commit(tip) else {
            return false;
        };
        let mut revwalk = match git.revwalk() {
            Ok(revwalk) => revwalk,
            Err(_) => return false,
        };
        if revwalk.push(commit.id()).is_err() {
            return false;
        }
        revwalk.any(|step| step.map(|step_oid| step_oid == oid).unwrap_or(false))
    }

    pub fn pristine(&self) -> &Pristine {
        &self.pristine
    }

    /// Read the legacy compatibility pointer. Persistent working-copy records
    /// are authoritative for all ordinary opens; this helper remains for native
    /// repair and deferred-TREE compatibility paths.
    fn read_current_view(dot_dir: &Path) -> Result<String, RepositoryError> {
        Ok(Self::read_current_view_if_present(dot_dir)?
            .unwrap_or_else(|| DEFAULT_STACK.to_string()))
    }

    fn read_current_view_if_present(dot_dir: &Path) -> Result<Option<String>, RepositoryError> {
        for name in ["current_view", "current_stack"] {
            let path = dot_dir.join(name);
            if path.exists() {
                let content = std::fs::read_to_string(path)?;
                let value = content.trim();
                if !value.is_empty() {
                    return Ok(Some(value.to_string()));
                }
            }
        }
        Ok(None)
    }

    fn read_legacy_current_view(
        working_copy_dot_dir: &Path,
        common_dot_dir: &Path,
    ) -> Result<String, RepositoryError> {
        if let Some(view) = Self::read_current_view_if_present(working_copy_dot_dir)? {
            return Ok(view);
        }
        if working_copy_dot_dir != common_dot_dir {
            if let Some(view) = Self::read_current_view_if_present(common_dot_dir)? {
                return Ok(view);
            }
        }
        Ok(DEFAULT_STACK.to_string())
    }

    /// Write the current view as a derived compatibility artifact.
    fn write_current_view(&self, view: &str) -> Result<(), RepositoryError> {
        working_copy::write_current_view_compatibility(&self.working_copy_dot_dir(), view)
    }

    /// Make prior atomic renames/removals in `.atomic` durable before a
    /// coupled database transition is allowed to advance. Directory fsync is
    /// available on Unix; other supported platforms retain atomic rename
    /// semantics but do not expose a portable directory durability barrier.
    fn sync_dot_dir(&self) -> Result<(), RepositoryError> {
        #[cfg(unix)]
        {
            std::fs::File::open(&self.dot_dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Get the path where a change file should be stored.
    ///
    /// Changes are stored in a two-level directory structure based on their hash:
    /// `.atomic/changes/AB/CDEF...` where AB is the first two characters of the
    /// base32-encoded hash.
    pub fn change_path(&self, hash_base32: &str) -> PathBuf {
        let prefix = &hash_base32[..2.min(hash_base32.len())];
        self.changes_dir().join(prefix).join(hash_base32)
    }

    /// Convert an absolute path to a repository-relative path.
    pub fn to_relative<P: AsRef<Path>>(&self, path: P) -> Option<PathBuf> {
        path.as_ref()
            .strip_prefix(&self.root)
            .ok()
            .map(Path::to_path_buf)
    }

    /// Convert a repository-relative path to an absolute path.
    pub fn to_absolute<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        self.root.join(path)
    }

    /// Check if a path is inside the .atomic directory.
    pub fn is_internal_path<P: AsRef<Path>>(&self, path: P) -> bool {
        path.as_ref().starts_with(&self.dot_dir)
    }
}
