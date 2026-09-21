//! Journaled projection publication (RFC §7.2 exit, §8.2, CB-8B ac-3).
//!
//! Every projection ref update and HEAD move is a **separate leased
//! resource**: the immutable operation (with per-effect expected-old/new
//! leases) is durable *before* any Git mutation, each effect classifies its
//! lease immediately before its write under the ordered operation lock,
//! performs a bounded compare-and-swap, and appends an immutable receipt
//! with the observed after-value. `observed == expected_new` is an
//! idempotent no-op; a third value appends a durable rejection receipt,
//! mutates nothing, and fails closed (RFC §12.6: divergence is explicit).
//!
//! Interruption leaves an incomplete operation head that gates ordinary
//! entry until recovery rolls the leases forward or back; the recovery
//! engine owns `refs/atomic/views/*`, `refs/heads/*`, and Git HEAD through
//! the same classification guard, so newer external work is preserved.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::operation::{EffectTarget, EffectValue, GitRefTarget, OperationKind};
use atomic_core::types::Base32;
use atomic_core::{Hash, WorkingCopyId};

use git2::Repository as GitRepository;

use super::locks::WorkingCopyOperationLockGuard;
use super::operation::{
    backup_entry_path, classify_effect_lease, git_object_hex, git_oid_from_object,
    write_new_synced, LeaseClassification,
};
use super::Repository;
use crate::RepositoryError;

/// Maximum bounded re-observations before refusing (RFC §7.1 step 8).
const MAX_REOBSERVATIONS: u32 = 3;

/// Reflog message prefix stamped on every ref/HEAD movement this executor
/// performs, tying the external write back to its journal entry.
const REFLOG_PREFIX: &str = "atomic:projection-publish";

/// Retained journal content names (CB-8B ac-3), under the operation's
/// recovery root: `entries/<ordinal>/{value,kind}`.
const RETAINED_VALUE_FILE: &str = "value";
const RETAINED_KIND_FILE: &str = "kind";
const RETAINED_ABSENT_FILE: &str = "absent";

/// A journaled projection publication awaiting its leased execution.
pub struct PreparedProjectionPublish {
    /// Immutable operation identity journaled before the Git effects.
    pub operation_id: atomic_core::OperationId,
    /// The ref this publication moves.
    pub ref_name: String,
    /// Intended ref target, when the ref effect is part of this operation.
    pub intended_ref: Option<GitRefTarget>,
    /// Intended HEAD target, when the HEAD effect is part of this operation.
    pub head_plan: Option<GitRefTarget>,
    /// Whether the checkpoint publication effect is part of this operation.
    pub checkpoint_plan: bool,
    /// Whether the index replacement effect is part of this operation.
    pub index_plan: bool,
    pub(super) lock: WorkingCopyOperationLockGuard,
}

fn git_map(message: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidRepository {
        reason: message.to_string(),
    }
}

fn direct_target(oid: git2::Oid) -> Result<GitRefTarget, RepositoryError> {
    let algorithm = match oid.as_bytes().len() {
        20 => atomic_core::operation::GitHashAlgorithm::Sha1,
        32 => atomic_core::operation::GitHashAlgorithm::Sha256,
        other => return Err(git_map(format!("unexpected Git OID width {other}"))),
    };
    let object = atomic_core::operation::GitObjectId::new(algorithm, oid.as_bytes().to_vec())
        .map_err(|error| git_map(error.to_string()))?;
    Ok(GitRefTarget::Direct(object))
}

fn read_ref(git: &GitRepository, name: &str) -> Result<Option<GitRefTarget>, RepositoryError> {
    match git.find_reference(name) {
        Ok(reference) => reference.target().map(direct_target).transpose(),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(git_map(format!("cannot read Git ref '{name}': {error}"))),
    }
}

/// Read the Git HEAD lease value: a direct target when detached, a symbolic
/// target when attached to an existing branch, and `Ok(None)` when HEAD is
/// missing or unborn (its symbolic target does not resolve). Operational
/// failures propagate as errors.
pub(super) fn read_head_target(
    git: &GitRepository,
) -> Result<Option<GitRefTarget>, RepositoryError> {
    match git.find_reference("HEAD") {
        Ok(head) => {
            if let Some(symref) = head.symbolic_target() {
                // An unborn branch (HEAD -> refs/heads/master with no
                // commits) resolves to no target: that is a missing lease
                // value, not an operational failure.
                return match git.find_reference(symref) {
                    Ok(_) => Ok(Some(GitRefTarget::Symbolic(symref.to_string()))),
                    Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
                    Err(error) => Err(git_map(format!("cannot read Git HEAD: {error}"))),
                };
            }
            head.target().map(direct_target).transpose()
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(git_map(format!("cannot read Git HEAD: {error}"))),
    }
}

/// Read the resolved commit identity HEAD currently points at, through its
/// symbolic chain. An unborn HEAD is `Ok(None)`.
fn read_head_resolved(git: &GitRepository) -> Result<Option<GitRefTarget>, RepositoryError> {
    match git.head() {
        Ok(head) => head.target().map(direct_target).transpose(),
        Err(error) if error.code() == git2::ErrorCode::UnbornBranch => Ok(None),
        Err(error) => Err(git_map(format!("cannot read Git HEAD: {error}"))),
    }
}

fn repository_object_algorithm(
    git: &GitRepository,
) -> Result<atomic_core::operation::GitHashAlgorithm, RepositoryError> {
    let value = match git
        .config()
        .and_then(|config| config.get_string("extensions.objectFormat"))
    {
        Ok(value) => value,
        Err(error) if error.code() == git2::ErrorCode::NotFound => "sha1".to_string(),
        Err(error) => return Err(git_map(format!("cannot read the object format: {error}"))),
    };
    match value.to_ascii_lowercase().as_str() {
        "sha1" => Ok(atomic_core::operation::GitHashAlgorithm::Sha1),
        "sha256" => Ok(atomic_core::operation::GitHashAlgorithm::Sha256),
        other => Err(git_map(format!("unsupported object format '{other}'"))),
    }
}

fn object_kind_name(kind: git2::ObjectType) -> &'static str {
    match kind {
        git2::ObjectType::Commit => "commit",
        git2::ObjectType::Tree => "tree",
        git2::ObjectType::Blob => "blob",
        _ => "unknown",
    }
}

fn object_kind_of_name(name: &str) -> Option<git2::ObjectType> {
    match name {
        "commit" => Some(git2::ObjectType::Commit),
        "tree" => Some(git2::ObjectType::Tree),
        "blob" => Some(git2::ObjectType::Blob),
        _ => None,
    }
}

/// Git's own `<kind> <len>\0<content>` object identity, recomputed
/// independently of the object database.
fn git_object_oid(kind: git2::ObjectType, content: &[u8]) -> atomic_core::operation::GitObjectId {
    use sha1::Digest as _;
    let mut hasher = sha1::Sha1::new();
    let header = format!("{} {}\0", object_kind_name(kind), content.len());
    hasher.update(header.as_bytes());
    hasher.update(content);
    let bytes: [u8; 20] = hasher.finalize().into();
    atomic_core::operation::GitObjectId::new(
        atomic_core::operation::GitHashAlgorithm::Sha1,
        bytes.to_vec(),
    )
    .expect("a SHA-1 digest is a valid 20-byte object id")
}

/// Observe one Git object as a lease value: `Absent`, or the object identity
/// authenticated by recomputing the stored bytes' hash (CB-8B ac-3: content
/// addressing is verified here, never assumed).
pub(super) fn observe_git_object_lease(
    git: &GitRepository,
    object: &atomic_core::operation::GitObjectId,
) -> Result<EffectValue, RepositoryError> {
    let oid = git_oid_from_object(git, object)?;
    let odb = git
        .odb()
        .map_err(|error| git_map(format!("cannot open the Git object database: {error}")))?;
    if !odb.exists(oid) {
        return Ok(EffectValue::Absent);
    }
    let object_hex = git_object_hex(object)?;
    let read = odb.read(oid).map_err(|error| {
        git_map(format!(
            "Git object {object_hex} is present but unreadable: {error}"
        ))
    })?;
    let Some(kind) = object_kind_of_name(object_kind_name(read.kind())) else {
        return Err(git_map(format!(
            "Git object {object_hex} carries an unsupported object kind {:?}",
            read.kind()
        )));
    };
    if git_object_oid(kind, read.data()).as_bytes() != object.as_bytes() {
        return Err(git_map(format!(
            "Git object {object_hex} is corrupt: the stored bytes do not hash to its identity"
        )));
    }
    Ok(EffectValue::GitObject(object.clone()))
}

/// Canonical, stat-independent lease observation of the Git index (CB-8B
/// ac-3): the sorted semantic entries and their tree identity, never the
/// on-disk bytes, so an external stat-cache refresh is not a third value.
///
/// The observation is taken from disk, not from the repository-cached index:
/// an external writer (`git add`, the executor's own aligned-index rename)
/// replaces the file without touching this handle, and a lease classification
/// against a stale snapshot would misclassify a third value as expected-old.
/// A force read reloads the file when present and explicitly clears to the
/// empty index when it is gone.
pub(super) fn observe_git_index_lease(
    git: &GitRepository,
) -> Result<atomic_core::operation::GitIndexState, RepositoryError> {
    let algorithm = repository_object_algorithm(git)?;
    let mut index = git
        .index()
        .map_err(|error| git_map(format!("cannot open the Git index for a lease: {error}")))?;
    index
        .read(true)
        .map_err(|error| git_map(format!("cannot refresh the Git index for a lease: {error}")))?;
    super::git_observation::observe_index_lease(algorithm, &index).map_err(git_map)
}

/// The clean stage-0 index lease of `tree`, derived in memory without
/// touching the on-disk index (deterministic: equal trees lease equal states).
pub(super) fn aligned_git_index_lease(
    git: &GitRepository,
    tree_oid: git2::Oid,
) -> Result<atomic_core::operation::GitIndexState, RepositoryError> {
    let tree = git.find_tree(tree_oid).map_err(|error| {
        git_map(format!(
            "cannot read the aligned projection tree for the index lease: {error}"
        ))
    })?;
    let mut index = git2::Index::new()
        .map_err(|error| git_map(format!("cannot open an in-memory index: {error}")))?;
    index
        .read_tree(&tree)
        .map_err(|error| git_map(format!("cannot derive the aligned index lease: {error}")))?;
    let algorithm = repository_object_algorithm(git)?;
    super::git_observation::observe_index_lease(algorithm, &index).map_err(git_map)
}

/// Facts digest of the parsed bridge checkpoint, `Ok(None)` when absent
/// (CB-8B ac-3: prepare and execute derive the Checkpoint lease identically
/// from the canonical checkpoint FACTS — never from file bytes — and a
/// missing checkpoint is a real first-publication lease value, not an error).
pub(super) fn observe_checkpoint_facts(root: &Path) -> Result<Option<Hash>, RepositoryError> {
    let Some(checkpoint) = super::workspace_txn::read_workspace_checkpoint(root)? else {
        return Ok(None);
    };
    let facts = super::adoption::checkpoint_facts_bytes(&checkpoint)?;
    Ok(Some(Hash::of(facts.as_slice())))
}

fn checkpoint_value(facts: Option<Hash>) -> EffectValue {
    facts
        .map(|hash| EffectValue::Digest {
            kind: atomic_core::operation::DigestKind::Checkpoint,
            hash,
        })
        .unwrap_or(EffectValue::Absent)
}

fn checkpoint_facts_digest(
    checkpoint: &super::workspace_txn::WorkspaceCheckpoint,
) -> Result<Hash, RepositoryError> {
    Ok(Hash::of(
        super::adoption::checkpoint_facts_bytes(checkpoint)?.as_slice(),
    ))
}

/// The checkpoint facts a projection publication plans to publish (CB-8B
/// ac-3): the prepared checkpoint is derived once at prepare time, retained
/// under the operation's recovery root, and re-verified against its FACTS
/// digest before the executor may publish it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionCheckpointPlan {
    /// The checkpoint's HEAD symref: `Some(branch ref)` for a Shared view's
    /// symbolic HEAD, `None` for a detached Draft/Ephemeral HEAD (RFC §8.1).
    pub git_head_symref: Option<String>,
    /// The projection commit HEAD moves to.
    pub git_head: atomic_core::operation::GitObjectId,
    /// The projection tree the commit carries.
    pub git_tree: atomic_core::operation::GitObjectId,
}

fn prepared_checkpoint_from_plan(
    view: &str,
    state: &atomic_core::types::Merkle,
    plan: &ProjectionCheckpointPlan,
    aligned_index: Option<&atomic_core::operation::GitIndexState>,
) -> Result<super::workspace_txn::WorkspaceCheckpoint, RepositoryError> {
    Ok(super::workspace_txn::WorkspaceCheckpoint {
        version: 2,
        view: view.to_string(),
        atomic_state: state.to_base32(),
        git_head_symref: plan.git_head_symref.clone(),
        git_head: git_object_hex(&plan.git_head)?,
        git_tree: git_object_hex(&plan.git_tree)?,
        git_index_tree: aligned_index
            .and_then(|index| index.tree.as_ref())
            .map(git_object_hex)
            .transpose()?,
        git_index_digest: aligned_index.map(|index| index.digest.to_base32()),
    })
}

impl Repository {
    /// Journal a projection publication before any Git mutation (RFC §7.2
    /// step 3/4, CB-8B ac-3, review ATOM::aaron::2).
    ///
    /// Every lease observation happens here, under the ordered operation
    /// lock. Effects, in stable ordinal order: the object creations (absent
    /// objects only — a present content-addressed object is already durable
    /// and idempotent), the index replacement, the ref update, the HEAD
    /// move, and the checkpoint publish. An already-current lease is a
    /// no-op with no effect plan (the operation codec refuses identical
    /// old/new leases), so a fully-current publication reports a zero
    /// operation id. The exact content the journal leases — the object
    /// bytes and the prepared checkpoint FACTS — is retained under the
    /// operation's recovery root before any Git mutation.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_projection_publication(
        &self,
        working_copy: WorkingCopyId,
        common: Option<&super::locks::RepositoryCommonLockGuard>,
        git: &GitRepository,
        ref_name: &str,
        intended_ref: Option<GitRefTarget>,
        intended_head: Option<GitRefTarget>,
        objects: Vec<(
            atomic_core::operation::GitObjectId,
            git2::ObjectType,
            Vec<u8>,
        )>,
        intended_index_tree: Option<git2::Oid>,
        checkpoint_plan: Option<ProjectionCheckpointPlan>,
        evidence: Hash,
    ) -> Result<PreparedProjectionPublish, RepositoryError> {
        let lock = match common {
            Some(common) => common.alias().try_lock_working_copy(working_copy)?,
            None => self.try_lock_operation(working_copy)?,
        };
        if let super::operation::OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let mut state = self.current_working_copy_state_for_projection(working_copy)?;
        // The checkpoint rollback path re-derives the pre-publication
        // checkpoint from the operation's before-Git facts, so the plan
        // carries them explicitly (CB-8B ac-3 recovery).
        let before_checkpoint = super::workspace_txn::read_workspace_checkpoint(self.root())?;
        if let Some(before) = &before_checkpoint {
            state.git = Some(super::adoption::checkpoint_git_state(before)?);
        }
        let view_name = state
            .view
            .as_ref()
            .map(|view| view.name.clone())
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: "projection publication requires a view state".to_string(),
            })?;

        let mut effects = Vec::new();
        let mut retained_objects = Vec::new();

        // ── Ordinals 0..n: the object creations (absent objects only). ─────
        {
            let odb = git.odb().map_err(|error| {
                git_map(format!("cannot open the Git object database: {error}"))
            })?;
            for (object, kind, bytes) in objects {
                let oid = git_oid_from_object(git, &object)?;
                if !odb.exists(oid) {
                    effects.push(atomic_core::operation::EffectPlan {
                        ordinal: effects.len() as u32,
                        target: EffectTarget::GitObject {
                            object: object.clone(),
                        },
                        expected_old: EffectValue::Absent,
                        expected_new: EffectValue::GitObject(object.clone()),
                    });
                    retained_objects.push((object, kind, bytes));
                }
            }
        }

        // ── The index replacement, when the index differs from the target. ──
        let mut aligned_index = None;
        if let Some(tree) = intended_index_tree {
            let observed = observe_git_index_lease(git)?;
            let aligned = aligned_git_index_lease(git, tree)?;
            if observed != aligned {
                effects.push(atomic_core::operation::EffectPlan {
                    ordinal: effects.len() as u32,
                    target: EffectTarget::GitIndex { working_copy },
                    expected_old: EffectValue::GitIndex(observed),
                    expected_new: EffectValue::GitIndex(aligned.clone()),
                });
                aligned_index = Some(aligned);
            }
        }

        // ── The ref update, when it actually moves the ref. ────────────────
        let observed_ref = read_ref(git, ref_name)?;
        if let Some(intended) = &intended_ref {
            if observed_ref.as_ref() != Some(intended) {
                effects.push(atomic_core::operation::EffectPlan {
                    ordinal: effects.len() as u32,
                    target: EffectTarget::GitRef {
                        name: ref_name.to_string(),
                    },
                    expected_old: observed_ref
                        .clone()
                        .map(EffectValue::GitRef)
                        .unwrap_or(EffectValue::Absent),
                    expected_new: EffectValue::GitRef(intended.clone()),
                });
            }
        }

        // ── The HEAD move, when it actually moves HEAD. ────────────────────
        let observed_head = read_head_target(git)?;
        if let Some(intended_head) = &intended_head {
            if observed_head.as_ref() != Some(intended_head) {
                effects.push(atomic_core::operation::EffectPlan {
                    ordinal: effects.len() as u32,
                    target: EffectTarget::GitHead { working_copy },
                    expected_old: observed_head
                        .clone()
                        .map(EffectValue::GitRef)
                        .unwrap_or(EffectValue::Absent),
                    expected_new: EffectValue::GitRef(intended_head.clone()),
                });
            }
        }

        // ── The checkpoint publish, when the live facts differ. ────────────
        let mut prepared_checkpoint = None;
        if let Some(plan) = &checkpoint_plan {
            let view_state = state.view.as_ref().map(|view| view.state).ok_or_else(|| {
                RepositoryError::InvalidOperation {
                    message: "projection checkpoint plan requires the view state".to_string(),
                }
            })?;
            let checkpoint = prepared_checkpoint_from_plan(
                &view_name,
                &view_state,
                plan,
                aligned_index.as_ref(),
            )?;
            let new_digest = checkpoint_facts_digest(&checkpoint)?;
            let observed_facts = observe_checkpoint_facts(self.root())?;
            if observed_facts.as_ref() != Some(&new_digest) {
                effects.push(atomic_core::operation::EffectPlan {
                    ordinal: effects.len() as u32,
                    target: EffectTarget::Checkpoint {
                        working_copy,
                        kind: atomic_core::operation::CheckpointKind::Bridge,
                    },
                    expected_old: checkpoint_value(observed_facts),
                    expected_new: EffectValue::Digest {
                        kind: atomic_core::operation::DigestKind::Checkpoint,
                        hash: new_digest,
                    },
                });
                prepared_checkpoint = Some(checkpoint);
            }
        }

        // The after-state carries the target checkpoint's Git facts so the
        // rollback re-derives the exact post-publication baseline (CB-8B).
        let mut after_state = state.clone();
        if let Some(checkpoint) = &prepared_checkpoint {
            after_state.git = Some(super::adoption::checkpoint_git_state(checkpoint)?);
        }
        if effects.is_empty() {
            // Nothing to move: every lease already holds the intended value.
            return Ok(PreparedProjectionPublish {
                operation_id: atomic_core::OperationId::from_bytes([0u8; 32]),
                ref_name: ref_name.to_string(),
                intended_ref: None,
                head_plan: None,
                checkpoint_plan: false,
                index_plan: false,
                lock,
            });
        }
        let prepared = self.prepare_working_copy_transition(
            &lock,
            OperationKind::ExportGitRefs,
            None,
            state,
            after_state,
            effects,
            vec![evidence],
            atomic_core::operation::ActorRef::System {
                name: "repository-projection-publish".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        let operation = prepared.operation();
        let operation_id = operation.id();
        let recovery_root = self.operation_recovery_root(working_copy, operation_id);

        // Retain the exact content the journal leased (CB-8B ac-3): the
        // object bytes and the prepared checkpoint FACTS, under the
        // operation's recovery root, before any Git mutation.
        for (object, kind, bytes) in &retained_objects {
            let Some(effect) = operation
                .payload()
                .delta
                .effects
                .iter()
                .find(|effect| {
                    matches!(&effect.target, EffectTarget::GitObject { object: candidate } if candidate == object)
                })
            else {
                return Err(RepositoryError::InvalidOperation {
                    message: "journaled object effect disappeared from the operation".to_string(),
                });
            };
            let entry = backup_entry_path(&recovery_root, effect.ordinal);
            fs::create_dir_all(&entry)
                .map_err(|error| git_map(format!("cannot retain the object bytes: {error}")))?;
            write_new_synced(&entry.join(RETAINED_VALUE_FILE), bytes)
                .map_err(|error| git_map(format!("cannot retain the object bytes: {error}")))?;
            write_new_synced(
                &entry.join(RETAINED_KIND_FILE),
                object_kind_name(*kind).as_bytes(),
            )
            .map_err(|error| git_map(format!("cannot retain the object kind: {error}")))?;
        }
        if let Some(checkpoint) = &prepared_checkpoint {
            let Some(effect) = operation
                .payload()
                .delta
                .effects
                .iter()
                .find(|effect| matches!(effect.target, EffectTarget::Checkpoint { .. }))
            else {
                return Err(RepositoryError::InvalidOperation {
                    message: "checkpoint effect missing from the journaled operation".to_string(),
                });
            };
            let entry = backup_entry_path(&recovery_root, effect.ordinal);
            fs::create_dir_all(&entry)
                .map_err(|error| git_map(format!("cannot retain the checkpoint: {error}")))?;
            let bytes = super::workspace_txn::workspace_checkpoint_bytes(checkpoint)?;
            write_new_synced(&entry.join(RETAINED_VALUE_FILE), &bytes)
                .map_err(|error| git_map(format!("cannot retain the checkpoint: {error}")))?;
        }

        let index_planned = operation
            .payload()
            .delta
            .effects
            .iter()
            .any(|effect| matches!(effect.target, EffectTarget::GitIndex { .. }));
        Ok(PreparedProjectionPublish {
            operation_id,
            ref_name: ref_name.to_string(),
            intended_ref: intended_ref.filter(|_| {
                operation
                    .payload()
                    .delta
                    .effects
                    .iter()
                    .any(|effect| matches!(effect.target, EffectTarget::GitRef { .. }))
            }),
            head_plan: intended_head.filter(|_| {
                operation
                    .payload()
                    .delta
                    .effects
                    .iter()
                    .any(|effect| matches!(effect.target, EffectTarget::GitHead { .. }))
            }),
            checkpoint_plan: prepared_checkpoint.is_some(),
            index_plan: index_planned,
            lock,
        })
    }

    /// Execute the journaled publication effects under their leases.
    ///
    /// Every planned effect — objects, index, refs, HEAD, and the checkpoint —
    /// runs in stable ordinal order with the same discipline: classify the
    /// lease BEFORE the write, perform a bounded compare-and-swap under the
    /// resource's own Git lock, and append an immutable receipt with the
    /// observed after-value. A third value records the rejection without any
    /// mutation and fails closed (RFC §12.6: divergence is explicit, never
    /// overwritten). Checkpoint-only, index-only, and object-only plans are
    /// complete publications on their own.
    pub fn execute_projection_publish(
        &self,
        prepared: &PreparedProjectionPublish,
        git: &GitRepository,
    ) -> Result<(), RepositoryError> {
        if prepared.operation_id == atomic_core::OperationId::from_bytes([0u8; 32]) {
            // Nothing was journaled: the publication is an idempotent no-op.
            return Ok(());
        }
        let operation = self.load_operation(prepared.operation_id)?;
        self.validate_projection_lock(&prepared.lock, &operation)?;
        let payload_effects = operation.payload().delta.effects.clone();
        for effect in &payload_effects {
            // Deterministic per-effect crash boundary (opt-in
            // instrumentation): after each effect's receipt, the named
            // failpoint turns the phase boundary into a typed failure so
            // reopen/retry recovery can be exercised for every effect.
            let failpoint = match &effect.target {
                EffectTarget::GitObject { .. } => Some("ATOMIC_FAIL_PROJECTION_AFTER_OBJECTS"),
                EffectTarget::GitIndex { .. } => Some("ATOMIC_FAIL_PROJECTION_AFTER_INDEX"),
                EffectTarget::GitRef { .. } => Some("ATOMIC_FAIL_PROJECTION_AFTER_REF"),
                EffectTarget::GitHead { .. } => Some("ATOMIC_FAIL_PROJECTION_AFTER_HEAD"),
                EffectTarget::Checkpoint { .. } => Some("ATOMIC_FAIL_PROJECTION_AFTER_CHECKPOINT"),
                _ => None,
            };
            match &effect.target {
                EffectTarget::GitObject { object } => {
                    self.execute_object_effect(&prepared.lock, &operation, effect, git, object)?
                }
                EffectTarget::GitIndex { .. } => {
                    self.execute_index_effect(&prepared.lock, &operation, effect, git)?
                }
                EffectTarget::GitRef { name } => {
                    self.execute_ref_effect(&prepared.lock, &operation, effect, git, name)?
                }
                EffectTarget::GitHead { .. } => {
                    self.execute_head_effect(&prepared.lock, &operation, effect, git)?
                }
                EffectTarget::Checkpoint {
                    kind: atomic_core::operation::CheckpointKind::Bridge,
                    ..
                } => self.execute_checkpoint_effect(&prepared.lock, &operation, effect, git)?,
                other => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "projection publication cannot execute effect target {other:?}"
                        ),
                    })
                }
            }
            projection_failpoint(failpoint)?;
        }
        Ok(())
    }

    /// Append the operation-level Verified receipt for a completed projection
    /// publication.
    ///
    /// Delegates to the recovery engine's verifier: every planned effect must
    /// already carry a successful immutable receipt, and every effect target —
    /// objects, index, refs, HEAD, and the checkpoint — must still observe its
    /// expected-new value before the operation may be verified (review
    /// ATOM::aaron::2 ac-3: no Verified before full equivalence).
    pub fn finalize_projection_publish(
        &self,
        prepared: PreparedProjectionPublish,
        git: &GitRepository,
    ) -> Result<atomic_core::OperationId, RepositoryError> {
        let _ = git;
        if prepared.operation_id == atomic_core::OperationId::from_bytes([0u8; 32]) {
            // Nothing was journaled: the publication is an idempotent no-op.
            return Ok(prepared.operation_id);
        }
        self.finalize_operation_verified(&prepared.lock, prepared.operation_id)?;
        Ok(prepared.operation_id)
    }

    /// One journaled Git object creation: verify the retained bytes hash to
    /// the leased identity, create the object, and receipt the outcome. An
    /// already-present object is idempotent (a `Recovered` receipt); a
    /// corrupt present object fails closed without mutation.
    fn execute_object_effect(
        &self,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
        effect: &atomic_core::operation::EffectPlan,
        git: &GitRepository,
        object: &atomic_core::operation::GitObjectId,
    ) -> Result<(), RepositoryError> {
        let observed = observe_git_object_lease(git, object)?;
        let EffectValue::GitObject(_) = &effect.expected_new else {
            return Err(RepositoryError::InvalidOperation {
                message: "Git object lease must expect an object value".to_string(),
            });
        };
        match classify_effect_lease(&observed, &effect.expected_old, &effect.expected_new) {
            LeaseClassification::AlreadyApplied | LeaseClassification::Diverged => {
                // AlreadyApplied → Recovered receipt; Diverged (a corrupt
                // object with the leased identity) → durable rejection and a
                // typed failure. No mutation either way.
                let value = observed.clone();
                self.record_effect_outcome(
                    lock,
                    operation.id(),
                    effect.ordinal,
                    value.clone(),
                    value,
                )?;
                return Ok(());
            }
            LeaseClassification::Apply => {}
        }
        let (bytes, kind) = self.load_retained_object(lock, operation, effect)?;
        if git_object_oid(kind, &bytes).as_bytes() != object.as_bytes() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "retained bytes for object {} do not hash to the leased identity",
                    git_object_hex(object)?
                ),
            });
        }
        let odb = git
            .odb()
            .map_err(|error| git_map(format!("cannot open the Git object database: {error}")))?;
        let written = odb
            .write(kind, &bytes)
            .map_err(|error| git_map(format!("cannot create the journaled Git object: {error}")))?;
        if written.as_bytes() != object.as_bytes() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "Git object database returned {written} for leased object {}",
                    git_object_hex(object)?
                ),
            });
        }
        let observed_after = observe_git_object_lease(git, object)?;
        self.record_effect_outcome(
            lock,
            operation.id(),
            effect.ordinal,
            EffectValue::Absent,
            observed_after,
        )?;
        Ok(())
    }

    /// One journaled index replacement under `.git/index.lock` (review
    /// ATOM::aaron::2 ac-2: expected-old index metadata compared while the
    /// index lock is held; a third value — external staged content — is
    /// preserved byte-for-byte and fails closed).
    fn execute_index_effect(
        &self,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
        effect: &atomic_core::operation::EffectPlan,
        git: &GitRepository,
    ) -> Result<(), RepositoryError> {
        let observed_before = observe_git_index_lease(git)?;
        match classify_effect_lease(
            &EffectValue::GitIndex(observed_before.clone()),
            &effect.expected_old,
            &effect.expected_new,
        ) {
            LeaseClassification::AlreadyApplied | LeaseClassification::Diverged => {
                // AlreadyApplied → Recovered; Diverged → durable rejection
                // that preserves the external staged content. No mutation.
                self.record_effect_outcome(
                    lock,
                    operation.id(),
                    effect.ordinal,
                    EffectValue::GitIndex(observed_before.clone()),
                    EffectValue::GitIndex(observed_before),
                )?;
                return Ok(());
            }
            LeaseClassification::Apply => {}
        }
        // Git's own index lockfile, held across the comparison, the content
        // retention, and the swap.
        let index_lock = match GitIndexLockGuard::acquire(git, MAX_REOBSERVATIONS)? {
            Some(lock) => lock,
            None => {
                return Err(git_map(
                    "Git index stayed contended past the bounded re-observation budget",
                ))
            }
        };
        // Bounded comparison under the lock: a value that moved away from
        // expected-old under us is refused, never overwritten.
        let current = observe_git_index_lease(git)?;
        if current != observed_before {
            // The index lock drops BEFORE the pristine write that appends
            // the rejection receipt — no final external-resource lock is
            // held across the journal append (canonical lock order, review
            // ATOM::aaron::2). The receipt records the actual under-lock
            // value as both observed sides: nothing was mutated.
            drop(index_lock);
            let value = EffectValue::GitIndex(current);
            self.record_effect_outcome(lock, operation.id(), effect.ordinal, value.clone(), value)?;
            return Ok(());
        }
        // Retain the exact current index bytes for the inverse recovery
        // before the swap (CB-8B ac-3: pre-write journal content retention).
        self.retain_current_index(git, lock, operation, effect)?;
        let EffectValue::GitIndex(aligned) = &effect.expected_new else {
            return Err(RepositoryError::InvalidOperation {
                message: "Git index lease expects a Git index value".to_string(),
            });
        };
        let Some(tree) = &aligned.tree else {
            return Err(RepositoryError::InvalidOperation {
                message: "the aligned index lease requires a resolvable tree".to_string(),
            });
        };
        let tree_oid = git_oid_from_object(git, tree)?;
        write_aligned_index(git, tree_oid)?;
        // The receipt is appended after the index lock is released: the
        // crash window between the swap and the receipt re-observes
        // `expected_new` and records the idempotent `Recovered` outcome on
        // retry — never an unconditional write presented as verified — and
        // the documented lock order (pristine write transaction before the
        // final external-resource locks) is kept structurally.
        drop(index_lock);
        let observed_after = observe_git_index_lease(git)?;
        self.record_effect_outcome(
            lock,
            operation.id(),
            effect.ordinal,
            EffectValue::GitIndex(observed_before),
            EffectValue::GitIndex(observed_after),
        )?;
        Ok(())
    }

    /// One journaled ref update through a real Git ref-transaction CAS: the
    /// ref's lockfile is held across the expected-old comparison and the
    /// move, so a concurrent writer's update fails with a locked-ref error
    /// instead of being overwritten (RFC §12.6, no force). A divergence found
    /// under the lock records the actual under-lock value in its durable
    /// receipt and is appended only after the transaction lock drops (review
    /// ATOM::aaron::2); `current == expected-new` under the lock is
    /// AlreadyApplied, not a false rejection.
    fn execute_ref_effect(
        &self,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
        effect: &atomic_core::operation::EffectPlan,
        git: &GitRepository,
        name: &str,
    ) -> Result<(), RepositoryError> {
        let observed = read_ref(git, name)?;
        let observed_value = observed
            .clone()
            .map(EffectValue::GitRef)
            .unwrap_or(EffectValue::Absent);
        match classify_effect_lease(&observed_value, &effect.expected_old, &effect.expected_new) {
            LeaseClassification::AlreadyApplied | LeaseClassification::Diverged => {
                // AlreadyApplied → Recovered; Diverged → the external
                // writer's value is preserved and the rejection is durable.
                self.record_effect_outcome(
                    lock,
                    operation.id(),
                    effect.ordinal,
                    observed_value.clone(),
                    observed_value,
                )?;
                return Ok(());
            }
            LeaseClassification::Apply => {}
        }
        let intended = effect_ref_target(effect)?;
        let GitRefTarget::Direct(ref intended) = intended else {
            return Err(RepositoryError::InvalidOperation {
                message: format!("projection ref '{name}' plan must be a direct object"),
            });
        };
        let intended_oid = git_oid_from_object(git, intended)?;
        let message = format!("{REFLOG_PREFIX} {}", operation.id());
        // Deterministic inside-window seam (opt-in instrumentation, review
        // ATOM::aaron::2): an external writer's value may land between the
        // lease's initial observation and the ref-transaction lock.
        inject_external_ref_before_lock(git)?;
        let mut applied = false;
        for _ in 0..MAX_REOBSERVATIONS {
            let mut transaction = git
                .transaction()
                .map_err(|error| git_map(format!("cannot open a Git ref transaction: {error}")))?;
            match transaction.lock_ref(name) {
                Ok(()) => {}
                Err(error) if error.code() == git2::ErrorCode::Locked => {
                    drop(transaction);
                    continue;
                }
                Err(error) => {
                    return Err(git_map(format!(
                        "cannot lock Git ref '{name}' for the journaled update: {error}"
                    )))
                }
            }
            // Under the lock: the bounded expected-old comparison.
            let current = read_ref(git, name)?;
            let current_value = current
                .clone()
                .map(EffectValue::GitRef)
                .unwrap_or(EffectValue::Absent);
            if current_value != effect.expected_old {
                // Nothing was mutated, so the durable outcome records the
                // ACTUAL under-lock value as both observed sides — never the
                // stale pre-lock observation, whose receipt would send
                // recovery after the wrong value (review ATOM::aaron::2).
                // The Git transaction lock drops BEFORE the pristine write
                // that appends the receipt (canonical lock order). A
                // concurrent writer that landed the intended value inside
                // the window is AlreadyApplied → a Recovered receipt and the
                // publication continues; a third value appends the
                // LeaseRejected receipt and fails closed.
                drop(transaction);
                self.record_effect_outcome(
                    lock,
                    operation.id(),
                    effect.ordinal,
                    current_value.clone(),
                    current_value,
                )?;
                return Ok(());
            }
            let write = transaction
                .set_target(name, intended_oid, None, &message)
                .and_then(|()| transaction.commit());
            match write {
                Ok(()) => {
                    applied = true;
                    break;
                }
                Err(error) => {
                    return Err(git_map(format!(
                        "cannot update projection ref '{name}': {error}"
                    )))
                }
            }
        }
        if !applied {
            return Err(git_map(format!(
                "Git ref '{name}' stayed contended past the bounded re-observation budget"
            )));
        }
        let observed_after = read_ref(git, name)?;
        self.record_effect_outcome(
            lock,
            operation.id(),
            effect.ordinal,
            observed_value,
            observed_after
                .map(EffectValue::GitRef)
                .unwrap_or(EffectValue::Absent),
        )?;
        Ok(())
    }

    /// One journaled HEAD move through a real Git ref-transaction CAS on
    /// `HEAD` (the same lockfile the git CLI honors), direct or symbolic.
    /// A divergence found under the lock records the actual under-lock value
    /// in its durable receipt and is appended only after the transaction
    /// lock drops (review ATOM::aaron::2); `current == expected-new` under
    /// the lock is AlreadyApplied, not a false rejection.
    fn execute_head_effect(
        &self,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
        effect: &atomic_core::operation::EffectPlan,
        git: &GitRepository,
    ) -> Result<(), RepositoryError> {
        let observed = read_head_target(git)?;
        let observed_value = observed
            .clone()
            .map(EffectValue::GitRef)
            .unwrap_or(EffectValue::Absent);
        match classify_effect_lease(&observed_value, &effect.expected_old, &effect.expected_new) {
            LeaseClassification::AlreadyApplied | LeaseClassification::Diverged => {
                self.record_effect_outcome(
                    lock,
                    operation.id(),
                    effect.ordinal,
                    observed_value.clone(),
                    observed_value,
                )?;
                return Ok(());
            }
            LeaseClassification::Apply => {}
        }
        let message = format!("{REFLOG_PREFIX} {}", operation.id());
        // Deterministic inside-window seam (opt-in instrumentation, review
        // ATOM::aaron::2): an external writer's value may land between the
        // lease's initial observation and the HEAD ref-transaction lock.
        inject_external_head_before_lock(git)?;
        let mut applied = false;
        for _ in 0..MAX_REOBSERVATIONS {
            let mut transaction = git
                .transaction()
                .map_err(|error| git_map(format!("cannot open a Git ref transaction: {error}")))?;
            match transaction.lock_ref("HEAD") {
                Ok(()) => {}
                Err(error) if error.code() == git2::ErrorCode::Locked => {
                    drop(transaction);
                    continue;
                }
                Err(error) => {
                    return Err(git_map(format!(
                        "cannot lock Git HEAD for the journaled move: {error}"
                    )))
                }
            }
            // Under the lock: the bounded expected-old comparison.
            let current = read_head_target(git)?;
            let current_value = current
                .clone()
                .map(EffectValue::GitRef)
                .unwrap_or(EffectValue::Absent);
            if current_value != effect.expected_old {
                // Nothing was mutated: the durable outcome records the ACTUAL
                // under-lock value as both observed sides — never the stale
                // pre-lock observation (review ATOM::aaron::2) — and the Git
                // transaction lock drops BEFORE the pristine write that
                // appends the receipt (canonical lock order). A concurrent
                // writer that landed the intended value inside the window is
                // AlreadyApplied → a Recovered receipt and the publication
                // continues; a third value appends the LeaseRejected receipt
                // and fails closed.
                drop(transaction);
                self.record_effect_outcome(
                    lock,
                    operation.id(),
                    effect.ordinal,
                    current_value.clone(),
                    current_value,
                )?;
                return Ok(());
            }
            let write = match &effect.expected_new {
                EffectValue::GitRef(GitRefTarget::Direct(object)) => {
                    let oid = git_oid_from_object(git, object)?;
                    transaction
                        .set_target("HEAD", oid, None, &message)
                        .and_then(|()| transaction.commit())
                }
                EffectValue::GitRef(GitRefTarget::Symbolic(symref)) => transaction
                    .set_symbolic_target("HEAD", symref, None, &message)
                    .and_then(|()| transaction.commit()),
                other => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "projection HEAD plan must carry a ref target, found {other:?}"
                        ),
                    })
                }
            };
            match write {
                Ok(()) => {
                    applied = true;
                    break;
                }
                Err(error) => return Err(git_map(format!("cannot move Git HEAD: {error}"))),
            }
        }
        if !applied {
            return Err(git_map(
                "Git HEAD stayed contended past the bounded re-observation budget",
            ));
        }
        let observed_after = read_head_target(git)?;
        self.record_effect_outcome(
            lock,
            operation.id(),
            effect.ordinal,
            observed_value,
            observed_after
                .map(EffectValue::GitRef)
                .unwrap_or(EffectValue::Absent),
        )?;
        Ok(())
    }

    /// One journaled checkpoint publish: the FACTS digest lease classifies
    /// before any mutation; the retained checkpoint bytes re-derive the exact
    /// lease digest (the unified prepare/execute facts encoding); the live
    /// Git facts are confirmed against the operation's after-state; the
    /// checkpoint facts are re-observed immediately before the write under
    /// the ordered operation guard (review ATOM::aaron::2); and only then are
    /// the canonical bytes written and receipted.
    fn execute_checkpoint_effect(
        &self,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
        effect: &atomic_core::operation::EffectPlan,
        git: &GitRepository,
    ) -> Result<(), RepositoryError> {
        let observed_facts = observe_checkpoint_facts(self.root())?;
        match classify_effect_lease(
            &checkpoint_value(observed_facts),
            &effect.expected_old,
            &effect.expected_new,
        ) {
            LeaseClassification::AlreadyApplied | LeaseClassification::Diverged => {
                // AlreadyApplied → Recovered; Diverged → the newer external
                // checkpoint bytes are preserved and the rejection durable.
                let value = checkpoint_value(observed_facts);
                self.record_effect_outcome(
                    lock,
                    operation.id(),
                    effect.ordinal,
                    value.clone(),
                    value,
                )?;
                return Ok(());
            }
            LeaseClassification::Apply => {}
        }
        // The retained checkpoint bytes re-derive the exact lease digest —
        // the unified facts encoding shared with prepare (CB-8B ac-3).
        let retained = self.load_retained_checkpoint(lock, operation, effect)?;
        let expected_digest = match &effect.expected_new {
            EffectValue::Digest { hash, .. } => *hash,
            other => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("checkpoint lease expects a digest, found {other:?}"),
                })
            }
        };
        if checkpoint_facts_digest(&retained)? != expected_digest {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "the retained checkpoint for operation {} does not re-derive the leased FACTS digest",
                    operation.id()
                ),
            });
        }
        // Verify the live Git facts against the operation's planned
        // after-state before publishing anything (RFC §7.2 step 6): a moved
        // HEAD/index/view publishes nothing.
        verify_checkpoint_live_facts(self, operation, git)?;
        // Deterministic inside-window seam (opt-in instrumentation, review
        // ATOM::aaron::2): an external writer may replace the checkpoint
        // after the executor's first read and before the guarded write.
        inject_external_checkpoint_before_write(self.root())?;
        // Re-observe the live checkpoint facts immediately before the write,
        // under the ordered operation guard (the CB-7B stable-observation
        // pattern): an external writer that replaced the checkpoint inside
        // the classify→write window holds newer bytes — the actual value is
        // receipted and nothing is overwritten. A window that landed the
        // intended value is AlreadyApplied → Recovered, not a second write.
        let reobserved_value = checkpoint_value(observe_checkpoint_facts(self.root())?);
        if reobserved_value != effect.expected_old {
            self.record_effect_outcome(
                lock,
                operation.id(),
                effect.ordinal,
                reobserved_value.clone(),
                reobserved_value,
            )?;
            return Ok(());
        }
        super::workspace_txn::write_workspace_checkpoint(self.root(), &retained)?;
        let observed_after = observe_checkpoint_facts(self.root())?;
        self.record_effect_outcome(
            lock,
            operation.id(),
            effect.ordinal,
            reobserved_value,
            checkpoint_value(observed_after),
        )?;
        Ok(())
    }
}

/// Verify the live Git facts match the operation's planned after-state before
/// the checkpoint may publish them (RFC §7.2 step 6): HEAD identity and
/// symref, view state, and the aligned index tree.
fn verify_checkpoint_live_facts(
    repo: &Repository,
    operation: &atomic_core::operation::Operation,
    git: &GitRepository,
) -> Result<(), RepositoryError> {
    let after = &operation.payload().delta.after;
    if let Some(view) = &after.view {
        let observed = repo.get_view_info(&view.name)?;
        if observed.state != view.state {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "checkpoint publication refused: view '{}' moved to {}, operation expected {}",
                    view.name, observed.state, view.state
                ),
            });
        }
    }
    let Some(git_state) = &after_git_state(operation) else {
        return Err(RepositoryError::InvalidOperation {
            message: "checkpoint effect carries no after-Git state".to_string(),
        });
    };
    let expected_head = match &git_state.head {
        atomic_core::operation::GitHeadState::Attached { oid, .. } => oid,
        atomic_core::operation::GitHeadState::Detached { oid } => oid,
        other => {
            return Err(RepositoryError::InvalidOperation {
                message: format!("checkpoint facts require a resolvable Git HEAD, found {other:?}"),
            })
        }
    };
    let observed_head = read_head_resolved(git)?;
    let GitRefTarget::Direct(observed_object) =
        observed_head.unwrap_or_else(|| GitRefTarget::Direct(expected_head.clone()))
    else {
        return Err(RepositoryError::InvalidOperation {
            message: "checkpoint publication refused: Git HEAD does not resolve to a commit"
                .to_string(),
        });
    };
    if observed_object != *expected_head {
        return Err(RepositoryError::InvalidOperation {
            message: "checkpoint publication refused: Git HEAD moved under the journal".to_string(),
        });
    }
    if let Some(index) = &git_state.index {
        if let Some(tree) = &index.tree {
            let observed_index = observe_git_index_lease(git)?;
            if observed_index.tree.as_ref() != Some(tree) {
                return Err(RepositoryError::InvalidOperation {
                    message: "checkpoint publication refused: the Git index does not hold the aligned projection tree".to_string(),
                });
            }
        }
    }
    Ok(())
}

fn effect_ref_target(
    effect: &atomic_core::operation::EffectPlan,
) -> Result<GitRefTarget, RepositoryError> {
    match &effect.expected_new {
        EffectValue::GitRef(target) => Ok(target.clone()),
        other => Err(RepositoryError::InvalidOperation {
            message: format!("projection ref plan must carry a ref target, found {other:?}"),
        }),
    }
}

fn after_git_state(
    operation: &atomic_core::operation::Operation,
) -> Option<&atomic_core::operation::GitStateRef> {
    operation.payload().delta.after.git.as_ref()
}

/// Deterministic per-effect crash boundary (opt-in instrumentation, CB-8B
/// ac-3): compiled only with the `adoption-test-injection` feature. Each
/// named environment variable turns the phase boundary after the effect's
/// receipt into a typed failure so reopen/retry recovery can be exercised
/// for every journaled effect. Shipping builds compile the no-op stub: no
/// environment value can alter publication behavior.
fn projection_failpoint(name: Option<&str>) -> Result<(), RepositoryError> {
    #[cfg(feature = "adoption-test-injection")]
    {
        if let Some(name) = name {
            if std::env::var_os(name).is_some() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("debug failpoint: {name}"),
                });
            }
        }
    }
    #[cfg(not(feature = "adoption-test-injection"))]
    {
        let _ = name;
    }
    Ok(())
}

/// Deterministic inside-window external-writer injections (opt-in
/// instrumentation, review ATOM::aaron::2): compiled only with the
/// `adoption-test-injection` feature. Each named environment variable makes
/// the executor itself land an external writer's value at the seam between
/// the lease's initial observation and its guarded write — the deterministic
/// stand-in for a concurrent writer whose interleaving is otherwise
/// unobservable in a single-threaded regression. Shipping builds compile the
/// no-op stubs: no environment value can alter publication behavior.
///
/// - `ATOMIC_TEST_INJECT_REF_BEFORE_LOCK` = `<ref-name>=<commit-oid-hex>`:
///   the ref is force-set to that value before the ref-transaction lock.
/// - `ATOMIC_TEST_INJECT_HEAD_BEFORE_LOCK` = `<commit-oid-hex>`: HEAD is
///   detached to that value before the HEAD ref-transaction lock.
/// - `ATOMIC_TEST_INJECT_CHECKPOINT_BEFORE_WRITE` = `<path>`: the checkpoint
///   file is replaced with the bytes at that path before the re-observation
///   guard and the canonical write.
fn inject_external_ref_before_lock(git: &GitRepository) -> Result<(), RepositoryError> {
    #[cfg(feature = "adoption-test-injection")]
    {
        if let Some(value) = std::env::var_os("ATOMIC_TEST_INJECT_REF_BEFORE_LOCK") {
            let spec = value.to_string_lossy();
            let Some((name, oid)) = spec.split_once('=') else {
                return Err(git_map(format!(
                    "debug injection expects '<ref-name>=<oid>', found '{spec}'"
                )));
            };
            let oid: git2::Oid = oid
                .parse()
                .map_err(|error| git_map(format!("debug injection oid '{oid}': {error}")))?;
            git.reference(
                name,
                oid,
                true,
                "external writer (injected inside the lease window)",
            )
            .map_err(|error| git_map(format!("debug injection ref '{name}': {error}")))?;
        }
    }
    #[cfg(not(feature = "adoption-test-injection"))]
    {
        let _ = git;
    }
    Ok(())
}

fn inject_external_head_before_lock(git: &GitRepository) -> Result<(), RepositoryError> {
    #[cfg(feature = "adoption-test-injection")]
    {
        if let Some(value) = std::env::var_os("ATOMIC_TEST_INJECT_HEAD_BEFORE_LOCK") {
            let oid: git2::Oid = value
                .to_string_lossy()
                .parse()
                .map_err(|error| git_map(format!("debug injection HEAD oid: {error}")))?;
            git.set_head_detached(oid)
                .map_err(|error| git_map(format!("debug injection detached HEAD: {error}")))?;
        }
    }
    #[cfg(not(feature = "adoption-test-injection"))]
    {
        let _ = git;
    }
    Ok(())
}

fn inject_external_checkpoint_before_write(root: &Path) -> Result<(), RepositoryError> {
    #[cfg(feature = "adoption-test-injection")]
    {
        if let Some(value) = std::env::var_os("ATOMIC_TEST_INJECT_CHECKPOINT_BEFORE_WRITE") {
            let path = PathBuf::from(value);
            let bytes = fs::read(&path).map_err(|error| {
                git_map(format!(
                    "debug injection checkpoint '{}': {error}",
                    path.display()
                ))
            })?;
            let target = root.join(super::workspace_txn::CHECKPOINT_RELATIVE_PATH);
            fs::write(&target, &bytes).map_err(|error| {
                git_map(format!(
                    "debug injection checkpoint '{}': {error}",
                    target.display()
                ))
            })?;
        }
    }
    #[cfg(not(feature = "adoption-test-injection"))]
    {
        let _ = root;
    }
    Ok(())
}

/// The Git index replacement lock: `.git/index.lock`, created exclusively and
/// held across the classify-compare-swap window. This is Git's own lockfile
/// protocol — external index writers (git CLI, libgit2) contend instead of
/// being overwritten.
struct GitIndexLockGuard {
    path: PathBuf,
}

impl GitIndexLockGuard {
    fn acquire(git: &GitRepository, attempts: u32) -> Result<Option<Self>, RepositoryError> {
        let path = git.path().join("index.lock");
        for attempt in 0..attempts {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Some(Self { path })),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt + 1 == attempts {
                        return Ok(None);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25 * (attempt as u64 + 1)));
                }
                Err(error) => {
                    return Err(git_map(format!(
                        "cannot create the Git index lock '{}': {error}",
                        path.display()
                    )))
                }
            }
        }
        Ok(None)
    }
}

impl Drop for GitIndexLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Replace the Git index with a clean stage-0 image of `tree` while the index
/// lock is held: serialize the aligned index beside the repository, then
/// rename it into place (Git's own replace protocol).
fn write_aligned_index(git: &GitRepository, tree_oid: git2::Oid) -> Result<(), RepositoryError> {
    let tree = git.find_tree(tree_oid).map_err(|error| {
        git_map(format!(
            "cannot read the aligned projection tree while replacing the index: {error}"
        ))
    })?;
    let staging = git
        .path()
        .join(format!("atomic-index-replace.{}.tmp", std::process::id()));
    let result = (|| -> Result<(), RepositoryError> {
        let mut index = git2::Index::open(&staging)
            .map_err(|error| git_map(format!("cannot stage the aligned index: {error}")))?;
        index
            .read_tree(&tree)
            .and_then(|()| index.write())
            .map_err(|error| git_map(format!("cannot serialize the aligned index: {error}")))?;
        fs::rename(&staging, git.path().join("index"))
            .map_err(|error| git_map(format!("cannot publish the aligned index: {error}")))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

impl Repository {
    /// The recovery root that retains one operation's journaled content.
    fn projection_recovery_root(
        &self,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
    ) -> PathBuf {
        self.operation_recovery_root(lock.working_copy(), operation.id())
    }

    /// Load one journaled object creation's retained bytes and kind (CB-8B
    /// ac-3): the exact content the immutable operation leased, retained
    /// under the recovery root before the object database write.
    fn load_retained_object(
        &self,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
        effect: &atomic_core::operation::EffectPlan,
    ) -> Result<(Vec<u8>, git2::ObjectType), RepositoryError> {
        let entry = backup_entry_path(
            &self.projection_recovery_root(lock, operation),
            effect.ordinal,
        );
        let bytes = fs::read(entry.join(RETAINED_VALUE_FILE)).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!(
                    "the retained object bytes for operation {} effect {} are missing: {error}",
                    operation.id(),
                    effect.ordinal
                ),
            }
        })?;
        let kind = fs::read_to_string(entry.join(RETAINED_KIND_FILE)).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!(
                    "the retained object kind for operation {} effect {} is missing: {error}",
                    operation.id(),
                    effect.ordinal
                ),
            }
        })?;
        let kind =
            object_kind_of_name(kind.trim()).ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "the retained object kind '{kind}' is unsupported for operation {}",
                    operation.id()
                ),
            })?;
        Ok((bytes, kind))
    }

    /// Retain the exact current index bytes for an index effect's inverse
    /// recovery (CB-8B ac-3: pre-write journal content retention).
    fn retain_current_index(
        &self,
        git: &GitRepository,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
        effect: &atomic_core::operation::EffectPlan,
    ) -> Result<(), RepositoryError> {
        let entry = backup_entry_path(
            &self.operation_recovery_root(lock.working_copy(), operation.id()),
            effect.ordinal,
        );
        fs::create_dir_all(&entry)
            .map_err(|error| git_map(format!("cannot retain the index backup: {error}")))?;
        match fs::read(git.path().join("index")) {
            Ok(bytes) => write_new_synced(&entry.join(RETAINED_VALUE_FILE), &bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_new_synced(&entry.join(RETAINED_VALUE_FILE), b"")?;
                write_new_synced(&entry.join(RETAINED_ABSENT_FILE), b"absent\n")
            }
            Err(error) => Err(git_map(format!(
                "cannot read the Git index for retention: {error}"
            ))),
        }
        .map_err(|error| git_map(format!("cannot retain the index bytes: {error}")))
    }

    /// Load one journaled checkpoint publication's retained canonical FACTS
    /// bytes and re-derive the checkpoint (CB-8B ac-3: the unified
    /// prepare/execute facts encoding).
    fn load_retained_checkpoint(
        &self,
        lock: &WorkingCopyOperationLockGuard,
        operation: &atomic_core::operation::Operation,
        effect: &atomic_core::operation::EffectPlan,
    ) -> Result<super::workspace_txn::WorkspaceCheckpoint, RepositoryError> {
        let entry = backup_entry_path(
            &self.operation_recovery_root(lock.working_copy(), operation.id()),
            effect.ordinal,
        );
        let bytes = fs::read(entry.join(RETAINED_VALUE_FILE)).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!(
                    "the retained checkpoint for operation {} effect {} is missing: {error}",
                    operation.id(),
                    effect.ordinal
                ),
            }
        })?;
        let checkpoint = super::workspace_txn::workspace_checkpoint_from_bytes(&bytes)?;
        let digest = checkpoint_facts_digest(&checkpoint)?;
        let expected = match &effect.expected_new {
            EffectValue::Digest { hash, .. } => *hash,
            other => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("checkpoint lease expects a digest, found {other:?}"),
                })
            }
        };
        if digest != expected {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "the retained checkpoint for operation {} does not re-derive the leased FACTS digest",
                    operation.id()
                ),
            });
        }
        Ok(checkpoint)
    }
}
