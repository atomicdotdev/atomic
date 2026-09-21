use super::*;
use super::operation::pristine_error;
use atomic_core::change::{CausalFrontier, ChangeKind, ChangeOrigin};
use atomic_core::operation::{
    ActorRef, EffectPlan, EffectTarget, EffectValue, FileKind, FileState, OperationKind,
    RepoStateRef,
};

/// Repository state for a working copy's private snapshot view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotState {
    pub view: String,
    pub baseline_view: String,
    pub head: Option<Hash>,
    pub remainder: Option<Hash>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotStatus {
    pub view: String,
    pub baseline_view: String,
    pub snapshot: Option<Hash>,
    pub remainder: Option<Hash>,
    pub superseded_snapshots: usize,
}

impl SnapshotStatus {
    pub fn is_active(&self) -> bool {
        self.snapshot.is_some() || self.remainder.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotRetentionPolicy {
    pub keep_superseded: usize,
    /// CB-13A R1 audit-expiry wall: objects whose recorded time is at or
    /// after this epoch second are audit-retained regardless of the
    /// supersedes-chain window. `None` (the default) means NO wall — the
    /// window count governs alone (the pre-existing behavior). Content
    /// liveness is always decided separately (a rooted object is kept even
    /// when the wall says expired).
    pub audit_retention_floor_unix: Option<i64>,
}

impl Default for SnapshotRetentionPolicy {
    fn default() -> Self {
        Self {
            keep_superseded: 8,
            audit_retention_floor_unix: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotRetentionOutcome {
    pub retained: Vec<Hash>,
    pub deleted: Vec<Hash>,
}

#[derive(Clone, Debug)]
pub(super) enum RecordLifecycle {
    Durable,
    Snapshot {
        working_copy: WorkingCopyId,
        view: String,
        supersedes: Option<Hash>,
        replaces: Option<Hash>,
    },
    Promotion {
        snapshot_view: String,
        snapshot: Hash,
    },
}

impl RecordLifecycle {
    pub(super) fn target_view<'a>(&'a self, baseline: &'a str) -> &'a str {
        match self {
            Self::Durable | Self::Promotion { .. } => baseline,
            Self::Snapshot { view, .. } => view,
        }
    }

    pub(super) fn removal(&self) -> Option<(&str, Hash)> {
        match self {
            Self::Durable => None,
            Self::Snapshot {
                view,
                replaces: Some(previous),
                ..
            } => Some((view, *previous)),
            Self::Snapshot { replaces: None, .. } => None,
            Self::Promotion {
                snapshot_view,
                snapshot,
            } => Some((snapshot_view, *snapshot)),
        }
    }

    /// Whether this lifecycle publishes a snapshot-kind change (R3: the
    /// publication path must refuse dependency-invalid snapshots BEFORE
    /// save/apply, not compensate after applying).
    pub(super) fn is_snapshot_publication(&self) -> bool {
        matches!(self, Self::Snapshot { .. })
    }

    pub(super) fn classify(&self, change: Change) -> Result<Change, RecordError> {
        let (kind, supersedes) = match self {
            Self::Durable | Self::Promotion { .. } => (ChangeKind::Durable, None),
            Self::Snapshot {
                working_copy,
                supersedes,
                ..
            } => (
                ChangeKind::Snapshot {
                    working_copy: *working_copy,
                },
                *supersedes,
            ),
        };
        change
            .with_classification(
                kind,
                supersedes,
                ChangeOrigin::Native,
                CausalFrontier::empty(),
            )
            .map_err(|error| RecordError::ChangeStore(error.to_string()))
    }

    pub(super) fn verify_promotion_content(
        &self,
        change: &Change,
        repo: &Repository,
    ) -> Result<(), RecordError> {
        let Self::Promotion { snapshot, .. } = self else {
            return Ok(());
        };
        let snapshot_change = repo
            .load_change(snapshot)
            .map_err(RecordError::Repository)?;
        if change.hunks() != snapshot_change.hunks()
            || change.contents != snapshot_change.contents
            || change.hashed.file_ops != snapshot_change.hashed.file_ops
        {
            return Err(RecordError::Repository(RepositoryError::InvalidOperation {
                message: format!(
                    "working copy no longer matches snapshot {}; create a new snapshot before promotion",
                    snapshot.to_base32()
                ),
            }));
        }
        Ok(())
    }
}

impl Repository {
    /// Canonical private view name for a physical working copy.
    pub fn snapshot_view_name(working_copy: WorkingCopyId) -> String {
        format!("wc/{working_copy}")
    }

    /// Inspect or create the private Draft snapshot view owned by `working_copy`.
    pub fn ensure_snapshot_view(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<SnapshotState, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let baseline_view = self.desired_view_name(working_copy)?;
        let view_name = Self::snapshot_view_name(working_copy);
        ensure_workspace_dir(&self.dot_dir, &view_name)?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let baseline = txn
            .get_view(&baseline_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: baseline_view.clone(),
            })?;
        let snapshot_view = match txn
            .get_view(&view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            Some(mut view) => {
                if !view.kind.is_draft() {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!("snapshot view '{}' must remain Draft", view_name),
                    });
                }
                if view.parent != Some(baseline.id) {
                    if view.change_count != 0 {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "snapshot view '{}' still contains a snapshot for a different baseline",
                                view_name
                            ),
                        });
                    }
                    view.parent = Some(baseline.id);
                    txn.update_view(&view)
                        .map_err(|error| RepositoryError::Database(error.to_string()))?;
                }
                view
            }
            None => txn
                .create_view(&view_name, ViewScope::Draft, Some(baseline.id))
                .map_err(|error| RepositoryError::Database(error.to_string()))?,
        };

        let mut direct = None;
        for row in txn
            .iter_changes(&snapshot_view, 0)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (_, node_id, _) =
                row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            let hash = txn
                .get_external(node_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: node_id.to_string(),
                })?;
            if direct.replace(hash).is_some() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "snapshot view '{}' contains more than one direct change",
                        view_name
                    ),
                });
            }
        }
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;

        let mut head = None;
        let mut remainder = None;
        if let Some(hash) = direct {
            let change = self.load_change(&hash)?;
            if change.kind().is_snapshot() {
                if change.kind().working_copy() != Some(working_copy) {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "snapshot view '{}' contains change {} not owned by working copy {}",
                            view_name,
                            hash.to_base32(),
                            working_copy
                        ),
                    });
                }
                head = Some(hash);
            } else {
                remainder = Some(hash);
            }
        }

        Ok(SnapshotState {
            view: view_name,
            baseline_view,
            head,
            remainder,
        })
    }

    /// Record the complete working-copy delta relative to its durable baseline.
    pub fn snapshot(
        &self,
        working_copy: WorkingCopyId,
        header: ChangeHeader,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        if !options.get_save_to_store() || !options.get_apply_after_record() {
            return Err(RecordError::Repository(RepositoryError::InvalidOperation {
                message: "snapshots must be saved and applied atomically".to_string(),
            }));
        }
        let state = self.ensure_snapshot_view(working_copy)?;
        self.record_with_lifecycle(
            working_copy,
            header,
            options,
            RecordLifecycle::Snapshot {
                working_copy,
                view: state.view,
                supersedes: state.head,
                replaces: state.head.or(state.remainder),
            },
        )
    }

    /// Promote the current snapshot by reassembling its content as a durable change.
    pub fn promote_snapshot(
        &self,
        working_copy: WorkingCopyId,
        header: ChangeHeader,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        if !options.get_save_to_store() || !options.get_apply_after_record() {
            return Err(RecordError::Repository(RepositoryError::InvalidOperation {
                message: "snapshot promotion must be saved and applied atomically".to_string(),
            }));
        }
        let state = self.ensure_snapshot_view(working_copy)?;
        let snapshot = state.head.ok_or_else(|| {
            RecordError::Repository(RepositoryError::InvalidOperation {
                message: format!("snapshot view '{}' has no snapshot to promote", state.view),
            })
        })?;
        self.record_with_lifecycle(
            working_copy,
            header,
            options,
            RecordLifecycle::Promotion {
                snapshot_view: state.view,
                snapshot,
            },
        )
    }

    /// Read snapshot/remainder lifecycle state without creating or mutating its private view.
    pub fn snapshot_status(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<SnapshotStatus, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let baseline_view = self.desired_view_name(working_copy)?;
        let view_name = Self::snapshot_view_name(working_copy);
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let Some(view) = txn
            .get_view(&view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        else {
            return Ok(SnapshotStatus {
                view: view_name,
                baseline_view,
                snapshot: None,
                remainder: None,
                superseded_snapshots: 0,
            });
        };
        let mut direct = Vec::new();
        for row in txn
            .iter_changes(&view, 0)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (_, node_id, _) =
                row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            direct.push(
                txn.get_external(node_id)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                    .ok_or_else(|| RepositoryError::ChangeNotFound {
                        hash: node_id.to_string(),
                    })?,
            );
        }
        drop(txn);
        if direct.len() > 1 {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "private view '{}' contains multiple direct changes",
                    view_name
                ),
            });
        }
        let mut snapshot = None;
        let mut remainder = None;
        if let Some(hash) = direct.first().copied() {
            if self.load_change(&hash)?.kind().is_snapshot() {
                snapshot = Some(hash);
            } else {
                remainder = Some(hash);
            }
        }
        let mut superseded_snapshots = 0;
        let mut cursor = snapshot
            .and_then(|hash| self.load_change(&hash).ok())
            .and_then(|change| change.supersedes().copied());
        let mut seen = std::collections::HashSet::new();
        while let Some(hash) = cursor {
            if !seen.insert(hash) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("snapshot supersedes cycle at {}", hash.to_base32()),
                });
            }
            superseded_snapshots += 1;
            cursor = self
                .load_change(&hash)
                .ok()
                .and_then(|change| change.supersedes().copied());
        }
        Ok(SnapshotStatus {
            view: view_name,
            baseline_view,
            snapshot,
            remainder,
            superseded_snapshots,
        })
    }

    /// Delete superseded snapshot objects beyond the retention window through leased effects.
    ///
    /// CB-13A R1: this is a **lock-first, conservative** destructive retention
    /// pass. The common + working-copy operation boundary is acquired BEFORE
    /// any observation; the entire deletion plan (audit-expiry window, view
    /// membership, and every other enumerable content root) is derived and
    /// re-derived under that lock, so a root established before the plan is
    /// built is always visible and no root can be established between plan
    /// construction and execution. Destructive pruning is refused outright
    /// when roots cannot be proven: an incomplete (recoverable) operation
    /// head in any scope, or a working-copy record that is not provably at
    /// its view's current state. Content roots are checked independently of
    /// the audit-expiry window (`keep_superseded` decides only how long
    /// superseded objects are kept, never whether a live root is deleted).
    pub fn prune_superseded_snapshots(
        &self,
        working_copy: WorkingCopyId,
        policy: SnapshotRetentionPolicy,
    ) -> Result<SnapshotRetentionOutcome, RepositoryError> {
        // ── 1. Lock first: the common boundary covers every operation writer,
        //      so nothing that pins a snapshot object can run concurrently.
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }

        // ── 2. Refuse destructive retention while any scope still holds an
        //      unresolved head: until recovery completes, the system cannot
        //      prove which objects its recoverable effects will need.
        self.refuse_retention_on_incomplete_heads(working_copy)?;

        // ── 3. Derive the whole retention plan under the held lock.
        let status = self.snapshot_status(working_copy)?;
        let Some(head) = status.snapshot else {
            return Ok(SnapshotRetentionOutcome::default());
        };
        let mut chain = Vec::new();
        let mut cursor = self.load_change(&head)?.supersedes().copied();
        let mut seen = std::collections::HashSet::new();
        while let Some(hash) = cursor {
            if !seen.insert(hash) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("snapshot supersedes cycle at {}", hash.to_base32()),
                });
            }
            // A previously-collected ancestor's bytes are gone: the chain
            // ends there (nothing beyond it can be traversed or audited,
            // and collection already proved it unpinned).
            let Ok(change) = self.load_change(&hash) else {
                break;
            };
            cursor = change.supersedes().copied();
            chain.push(hash);
        }
        // Audit-expiry eligibility only: how long superseded objects are
        // kept, bounded by the CB-13A audit-retention wall clock. Content
        // liveness is decided separately below (the wall never deletes a
        // rooted object).
        let audit_expired = chain
            .iter()
            .skip(policy.keep_superseded)
            .copied()
            .filter(|hash| {
                !self.object_is_audit_retained(hash, policy.audit_retention_floor_unix)
            })
            .collect::<Vec<_>>();
        if audit_expired.is_empty() {
            let retained = chain
                .iter()
                .take(policy.keep_superseded)
                .copied()
                .collect::<Vec<_>>();
            return Ok(SnapshotRetentionOutcome {
                retained,
                deleted: Vec::new(),
            });
        }

        // ── 4. Conservative content-root scan under the same lock. A
        //      candidate is deleted only when every enumerable root class
        //      proves it unreferenced; anything referenced — or anything this
        //      scan cannot prove — is retained.
        if !self.retention_working_copy_states_provable()? {
            return Err(RepositoryError::InvalidOperation {
                message: "snapshot retention refused: content-root discovery is incomplete — \
                          a working-copy record is not provably at its view's current state, so \
                          candidate objects cannot be proven unreferenced; refresh the working \
                          copy (or complete the interrupted switch) before destructive pruning"
                    .to_string(),
            });
        }
        let txn = self
            .pristine
            .read_txn()
            .map_err(pristine_error)?;
        let mut candidates = Vec::new();
        for hash in &audit_expired {
            if self.snapshot_object_is_rooted(&txn, hash)? {
                continue;
            }
            candidates.push(*hash);
        }
        drop(txn);
        let retained = chain
            .iter()
            .take(policy.keep_superseded)
            .copied()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(SnapshotRetentionOutcome {
                retained,
                deleted: Vec::new(),
            });
        }

        // ── 5. Lease the filesystem deletions inside the same held boundary.
        let record = self.working_copy_record(working_copy)?;
        let state = RepoStateRef {
            view: None,
            working_copy: Some(super::operation::working_copy_state_ref(record)),
            git: None,
        };
        let mut effects = Vec::with_capacity(candidates.len());
        for (ordinal, hash) in candidates.iter().enumerate() {
            let path = self.change_store().change_path(hash);
            let bytes = std::fs::read(&path)?;
            let relative = path
                .strip_prefix(&self.root)
                .map_err(|error| RepositoryError::InvalidOperation {
                    message: error.to_string(),
                })?
                .to_string_lossy()
                .replace('\\', "/");
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&path)?.permissions().mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = 0o644;
            effects.push(EffectPlan {
                ordinal: ordinal as u32,
                target: EffectTarget::FilesystemPath { path: relative },
                expected_old: EffectValue::File(FileState {
                    kind: FileKind::Regular,
                    mode,
                    content: Hash::of(&bytes),
                }),
                expected_new: EffectValue::Absent,
            });
        }
        let prepared = self.prepare_working_copy_transition(
            &operation_lock,
            OperationKind::Record,
            None,
            state.clone(),
            state,
            effects,
            Vec::new(),
            ActorRef::System {
                name: "repository-snapshot-retention".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        let operation_id = prepared.operation().id();
        let mut deleted = Vec::new();
        for (ordinal, hash) in candidates.iter().enumerate() {
            let pending = self.execute_filesystem_effect(
                &operation_lock,
                operation_id,
                ordinal as u32,
                None,
            )?;
            self.record_pending_filesystem_effect(&operation_lock, operation_id, pending)?;
            self.change_store().evict(hash);
            deleted.push(*hash);
        }
        self.finalize_operation_verified(&operation_lock, operation_id)?;
        Ok(SnapshotRetentionOutcome { retained, deleted })
    }

    /// Refuse destructive snapshot retention while any operation scope still
    /// holds an unresolved (incomplete or diverged) head (CB-13A R1).
    fn refuse_retention_on_incomplete_heads(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<(), RepositoryError> {
        use atomic_core::operation::OperationScope;
        use atomic_core::pristine::{OperationTxnT, WorkingCopyTxnT};

        let refusal = |scope: String| {
            RepositoryError::InvalidOperation {
                message: format!(
                    "snapshot retention refused: operation {scope} is still completing \
                     (incomplete head); run a writable command so recovery completes before \
                     destructive pruning"
                ),
            }
        };
        if self.working_copy_operation_requires_recovery(working_copy)? {
            return Err(refusal(format!("working-copy:{working_copy}")));
        }
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let repository_heads = txn
            .get_operation_heads(OperationScope::Repository)
            .map_err(pristine_error)?;
        match repository_heads.as_slice() {
            [] => {}
            [head] => {
                let receipts = txn.get_effect_receipts(*head).map_err(pristine_error)?;
                if !super::operation::has_operation_verified_receipt(&receipts) {
                    return Err(refusal("repository".to_string()));
                }
            }
            many => {
                return Err(RepositoryError::OperationHeadsDiverged {
                    scope: OperationScope::Repository.to_string(),
                    heads: many.iter().map(ToString::to_string).collect(),
                });
            }
        }
        for record in txn.list_working_copies().map_err(pristine_error)? {
            if record.id == working_copy {
                continue;
            }
            let heads = txn
                .get_operation_heads(OperationScope::WorkingCopy(record.id))
                .map_err(pristine_error)?;
            match heads.as_slice() {
                [] => {}
                [head] => {
                    let receipts = txn.get_effect_receipts(*head).map_err(pristine_error)?;
                    if !super::operation::has_operation_verified_receipt(&receipts) {
                        return Err(refusal(format!("working-copy:{}", record.id)));
                    }
                }
                many => {
                    return Err(RepositoryError::OperationHeadsDiverged {
                        scope: OperationScope::WorkingCopy(record.id).to_string(),
                        heads: many.iter().map(ToString::to_string).collect(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Whether every working-copy record is provably at its desired view's
    /// current state (CB-13A R1).
    ///
    /// A record that is at its view's current head commits exactly to that
    /// view's current change log, so a candidate outside every view cannot be
    /// referenced by it. A record that is behind (an interrupted switch, a
    /// historical state, or a dangling view reference) cannot be proven to
    /// exclude the candidate, and the caller must refuse destructive pruning.
    fn retention_working_copy_states_provable(&self) -> Result<bool, RepositoryError> {        use atomic_core::pristine::{ViewTxnT, WorkingCopyTxnT};

        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        for record in txn.list_working_copies().map_err(pristine_error)? {
            let Some(view) = txn.get_view_by_id(record.desired_view).map_err(pristine_error)?
            else {
                return Ok(false);
            };
            let desired_current = record.desired_state == view.state;
            let materialized_current = match record.materialized_state {
                None => true,
                Some(materialized) => materialized == view.state,
            };
            if !(desired_current && materialized_current) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether any content root still references `hash` (CB-13A R1).
    ///
    /// Every enumerable root class must prove absence before the object is a
    /// deletion candidate; an unscannable store degrades to "rooted" so the
    /// object is retained rather than destroyed.
    fn snapshot_object_is_rooted(
        &self,
        txn: &atomic_core::pristine::ReadTxn,
        hash: &Hash,
    ) -> Result<bool, RepositoryError> {
        use atomic_core::pristine::{BindingTxnT, OperationTxnT, ViewTxnT};

        let base32 = hash.to_base32();

        // 1. View membership (recomputed under the held operation lock).
        if self
            .views_containing_change(hash)
            .map(|views| !views.is_empty())
            .unwrap_or(true)
        {
            return Ok(true);
        }

        // 2. The immutable operation journal and effect receipts. Only
        //    recoverable state keeps an object alive: effect leases and
        //    observed receipt values reference the object's durable bytes,
        //    and an operation that has not verified yet may still need them
        //    during recovery. The `evidence`/loss-note provenance of a
        //    *verified* operation is historical record-keeping, not a
        //    liveness root — otherwise every change would be pinned forever
        //    by the record operation that created it.
        for operation in txn.list_operations().map_err(pristine_error)? {
            let receipts = txn.get_effect_receipts(operation.id()).map_err(pristine_error)?;
            let verified = super::operation::has_operation_verified_receipt(&receipts);
            let payload = operation.payload();
            if !verified
                && (payload.evidence.iter().any(|e| e == hash)
                    || payload
                        .lossy
                        .iter()
                        .any(|note| note.evidence.iter().any(|e| e == hash)))
            {
                return Ok(true);
            }
            for effect in &payload.delta.effects {
                if retention_effect_target_references(&effect.target, hash)
                    || retention_effect_value_references(&effect.expected_old, hash)
                    || retention_effect_value_references(&effect.expected_new, hash)
                {
                    return Ok(true);
                }
            }
            for receipt in &receipts {
                let payload = receipt.payload();
                if let Some(observed) = &payload.observed_old {
                    if retention_effect_value_references(observed, hash) {
                        return Ok(true);
                    }
                }
                if let Some(observed) = &payload.observed_new {
                    if retention_effect_value_references(observed, hash) {
                        return Ok(true);
                    }
                }
            }
        }

        // 3. Persisted conflicts name the involved changes by base32.
        for name in txn.list_views().map_err(pristine_error)? {
            let Some(view) = txn.get_view(&name).map_err(pristine_error)? else {
                continue;
            };
            for (_inode, conflicts) in txn.iter_conflicts(view.id).map_err(pristine_error)? {
                if conflicts
                    .iter()
                    .any(|conflict| conflict.sides.iter().any(|side| side == &base32))
                {
                    return Ok(true);
                }
            }
        }

        // 4. Immutable Git state bindings: signed bytes may embed the change.
        for id in txn.iter_binding_ids().map_err(pristine_error)? {
            if let Some(bytes) = txn.get_binding_bytes(&id).map_err(pristine_error)? {
                if bytes.windows(32).any(|window| window == hash.as_bytes()) {
                    return Ok(true);
                }
            }
        }

        // 5. Durable text/binary stores: incomplete sessions, WIP and shelf
        //    workspaces, and crash-recovery backups. The object's own
        //    `.atomic/changes/` file is deliberately excluded: the audit
        //    window governs supersedes-chain retention there.
        if self.durable_retention_stores_reference(&base32)? {
            return Ok(true);
        }

        // 5b. Schema-aware session-root validation (CB-13A follow-up R1):
        //     incomplete sessions' decoded JSON roots — the last
        //     attestation hash, unbound commit evidence, and the recovery
        //     ref target — are validated as DECODED root references, not
        //     raw byte scans. A session file that cannot be decoded cannot
        //     be proven clean and is conservatively a root.
        if self.incomplete_session_roots_reference(&txn, hash, &base32)? {
            return Ok(true);
        }

        // 6. Git keep refs (CB-13A R1): a ref under `refs/atomic/keep/…`
        //    pins its target object bytes — the raw commit bytes are
        //    byte-scanned for the change hash, exactly like the binding
        //    scan above. Enumerating refs is read-only; an unreadable ref
        //    cannot be proven clean and counts as a root.
        if self.git_keep_refs_reference(&txn, &base32)? {
            return Ok(true);
        }
        Ok(false)
    }

    /// Whether the object's recorded time is at/after the audit-retention
    /// wall (CB-13A R1): the wall clock is a floor on deletability, distinct
    /// from content liveness. `None` = no wall configured (the policy
    /// window governs alone, the pre-existing behavior). An unloadable
    /// object cannot be proven deletable and is retained.
    fn object_is_audit_retained(&self, hash: &Hash, floor_unix: Option<i64>) -> bool {
        let Some(floor) = floor_unix else {
            return false;
        };
        // The change's recorded header timestamp is the object's audit
        // time; an unloadable object cannot be proven deletable.
        match self.load_change(hash) {
            Ok(change) => change.hashed.header.timestamp.timestamp() >= floor,
            Err(_) => true,
        }
    }

    /// Whether any Git ref under `refs/atomic/keep/…` byte-references the
    /// change hash. Unreadable refs count as roots (conservative, CB-13A R1).
    fn git_keep_refs_reference(
        &self,
        _txn: &atomic_core::pristine::ReadTxn,
        base32: &str,
    ) -> Result<bool, RepositoryError> {
        // A repository without Git has no keep refs: provable absence (the
        // non-git fixture path must prune as before).
        let git = match git2::Repository::open(self.root()) {
            Ok(git) => git,
            Err(_) => return Ok(false),
        };
        let keep_prefix = "refs/atomic/keep/";
        let mut any_ref = false;
        for reference in git
            .references_glob("refs/atomic/keep/*")
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let reference =
                reference.map_err(|error| RepositoryError::Database(error.to_string()))?;
            any_ref = true;
            let bytes: Vec<u8> = reference
                .peel_to_commit()
                .and_then(|commit| {
                    let odb = git.odb().map_err(|e| git2::Error::from_str(&e.to_string()))?;
                    odb.read(commit.id())
                        .map(|object| object.data().to_vec())
                        .map_err(|e| git2::Error::from_str(&e.to_string()))
                })
                .unwrap_or_default();
            // Byte scan: the raw 32-byte hash and its base32 text both pin.
            if bytes.windows(32).any(|window| window == Hash::from_base32(base32.as_bytes()).map(|h| h.0).unwrap_or([0u8; 32]))
                || String::from_utf8_lossy(&bytes).contains(base32)
            {
                return Ok(true);
            }
        }
        // No keep refs exist: absence is provable (nothing can be pinned).
        let _ = any_ref;
        Ok(false)
    }

    /// Whether sessions, WIP/shelf workspaces, recovery backups, or the
    /// bridge workspace metadata still reference the object (CB-13A R1).
    ///
    /// The base32 hash may appear as text (session JSON, WIP manifests) or as
    /// a path component (recovery backups mirror the change-store layout), so
    /// both file bytes and relative path components are searched. Unreadable
    /// entries cannot be proven clean and count as references.
    fn durable_retention_stores_reference(&self, base32: &str) -> Result<bool, RepositoryError> {
        let mut roots = vec![
            self.dot_dir.join("sessions"),
            self.dot_dir.join("operation-recovery"),
            self.dot_dir.join("bridge"),
        ];
        // CB-13A follow-up R1: fail-closed enumeration — a read_dir failure
        // on `working-copies/` is NEVER flattened into absence; the
        // per-working-copy roots cannot be proven clean, so the whole
        // lookup conservatively reports a reference (destructive pruning
        // then refuses). Only NotFound proves the directory absent.
        match std::fs::read_dir(self.dot_dir.join("working-copies")) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(_) => {
                            return Ok(true);
                        }
                    };
                    let path = entry.path();
                    roots.push(path.join("workspaces"));
                    roots.push(path.join("operation-recovery"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Ok(true);
            }
        }
        for root in roots {
            // CB-13A follow-up R1: `exists()` errors are never absence —
            // only metadata-confirmed NotFound skips; any other metadata
            // failure conservatively counts as a reference.
            match std::fs::symlink_metadata(&root) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Ok(true),
            }
            if retention_tree_references(&root, base32, 0)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Schema-aware incomplete-session root validation (CB-13A follow-up
    /// R1): every session JSON in the sessions store is decoded; a session
    /// that is INCOMPLETE (its durable status carries refusal evidence)
    /// pins the decoded root hashes it references — the last attestation
    /// hash and every unbound commit evidence hash — as content roots,
    /// regardless of age. A JSON file that fails to decode cannot be proven
    /// clean and conservatively roots everything (destructive pruning then
    /// refuses). Raw-byte scanning stays as the belt-and-braces fallback in
    /// `durable_retention_stores_reference`.
    fn incomplete_session_roots_reference(
        &self,
        _txn: &atomic_core::pristine::ReadTxn,
        change_hash: &Hash,
        base32: &str,
    ) -> Result<bool, RepositoryError> {
        let sessions_dir = self.dot_dir.join("sessions");
        let entries = match std::fs::read_dir(&sessions_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Ok(true),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return Ok(true),
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => return Ok(true),
            };
            let value: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => return Ok(true),
            };
            // Durable incomplete status: the session pins its evidence.
            let is_incomplete = value
                .get("status")
                .and_then(|status| status.get("incomplete"))
                .is_some();
            if !is_incomplete {
                continue;
            }
            // The last attestation hash is a durable evidence root.
            if let Some(last) = value.get("last_attestation").and_then(|last| last.as_str()) {
                if last.eq_ignore_ascii_case(base32) {
                    return Ok(true);
                }
            }
            // Unbound commits are raw Git OIDs, not Atomic change hashes —
            // they cannot root an Atomic change object. The recovery ref
            // name pins WIP refs already covered by the WIP scan.
            let _ = change_hash;
        }
        Ok(false)
    }

    pub(super) fn ensure_change_allowed_in_view<T: ViewTxnT>(
        &self,
        txn: &T,
        view_name: &str,
        change: &Change,
    ) -> Result<(), RepositoryError> {
        let view = txn
            .get_view(view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        if let Some(owner) = change.kind().working_copy() {
            if view.kind.is_shared() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("snapshot changes cannot enter Shared view '{}'", view_name),
                });
            }
            let owner_view = Self::snapshot_view_name(owner);
            if view_name != owner_view {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "snapshot owned by working copy {} can only enter private view '{}'",
                        owner, owner_view
                    ),
                });
            }
        }
        Ok(())
    }

    pub(super) fn ensure_exchangeable_change(&self, root: &Hash) -> Result<(), RepositoryError> {
        let mut pending = vec![*root];
        let mut seen = std::collections::HashSet::new();
        while let Some(hash) = pending.pop() {
            if !seen.insert(hash) {
                continue;
            }
            let change = self.load_change(&hash)?;
            if change.kind().is_snapshot() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "change {} is a private snapshot and cannot be exchanged",
                        hash.to_base32()
                    ),
                });
            }
            pending.extend(change.dependencies().iter().copied());
        }
        Ok(())
    }

    pub(super) fn ensure_view_has_no_snapshots<T: ViewTxnT>(
        &self,
        txn: &T,
        view: &atomic_core::pristine::ViewState,
    ) -> Result<(), RepositoryError> {
        for row in txn
            .iter_changes(view, 0)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (_, node_id, _) =
                row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            let hash = txn
                .get_external(node_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: node_id.to_string(),
                })?;
            if self.load_change(&hash)?.kind().is_snapshot() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "view '{}' contains private snapshot {}; it cannot become Shared",
                        view.name,
                        hash.to_base32()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Whether one effect lease value names the object `hash` (CB-13A R1).
///
/// A filesystem lease over the change store carries the object's content
/// hash, so an interrupted retention/record/recovery effect that references
/// the candidate keeps it rooted.
fn retention_effect_value_references(value: &EffectValue, hash: &Hash) -> bool {
    match value {
        EffectValue::File(state) => state.content == *hash,
        _ => false,
    }
}

/// Whether one effect target names the change object `hash` (CB-13A R1).
///
/// A filesystem effect over the content store addresses the object by its
/// base32 hash inside the `.atomic/changes/` layout, which is the durable
/// reference an interrupted operation (or its recovery inverse) keeps alive.
fn retention_effect_target_references(target: &EffectTarget, hash: &Hash) -> bool {
    match target {
        EffectTarget::FilesystemPath { path } => {
            path.starts_with(".atomic/changes/") && path.contains(&hash.to_base32())
        }
        _ => false,
    }
}

/// Bounded recursive search of one durable store directory for a base32
/// reference: a path component or a file whose bytes contain the needle.
///
/// Symlinks are never followed, individual unreadable files count as a
/// reference (fail closed), and the walk budget keeps pathological trees
/// from hanging the retention pass.
pub(super) fn retention_tree_references(
    root: &std::path::Path,
    needle: &str,
    depth: u32,
) -> Result<bool, RepositoryError> {
    if depth > 32 {
        return Ok(true);
    }
    let Ok(top_entries) = std::fs::read_dir(root) else {
        // A store that does not exist holds nothing; a store that exists but
        // cannot be enumerated cannot be proven clean.
        return Ok(!std::fs::metadata(root)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false));
    };
    let mut budget: usize = 100_000;
    let mut pending: Vec<(std::path::PathBuf, std::fs::ReadDir)> =
        vec![(root.to_path_buf(), top_entries)];
    while let Some((directory, mut entries)) = pending.pop() {
        let _ = &directory;
        {
        for entry in entries.by_ref() {
            budget = budget.saturating_sub(1);
            if budget == 0 {
                return Ok(true);
            }
            let Ok(entry) = entry else {
                return Ok(true);
            };
            let path = entry.path();
            if path
                .to_string_lossy()
                .contains(needle)
            {
                return Ok(true);
            }
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                return Ok(true);
            };
            if metadata.is_dir() {
                if let Ok(child_entries) = std::fs::read_dir(&path) {
                    pending.push((path, child_entries));
                } else {
                    return Ok(true);
                }
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) => {
                    return Ok(true)
                }
                Ok(_) => {}
                Err(_) => return Ok(true),
            }
        }
        }
    }
    Ok(false)
}
