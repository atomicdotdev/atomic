//! Scoped, journaled cleanup of *stale* persisted conflict rows.
//!
//! `materialize` persists conflict rows into the `CONFLICTS` table so `status`
//! can report a conflicted file without re-rendering the graph. A row is
//! **stale** when the canonical graph render for the path no longer emits a
//! real conflict *and* the rendered bytes equal the working-tree bytes: the
//! conflict was resolved upstream (or the row was persisted for a state the
//! view no longer renders), but nothing has refreshed the table, so the row
//! outlives the conflict it describes.
//!
//! Such a row is invisible to `record` (the file is unchanged, so the change
//! is skipped) yet it is not a genuine conflict. Without a supported path to
//! clear it, the row makes `record` a permanent no-op for the path.
//!
//! This module implements that supported path. It is deliberately narrow:
//!
//! 1. only **explicitly named** paths are considered — never `--all`;
//! 2. a path is only clearable when the canonical render emits no conflict
//!    region and its bytes equal the working-tree bytes (a genuine graph
//!    conflict, diverged content, or a name conflict is never cleared);
//! 3. the mutation runs inside one immediately-durable write transaction under
//!    the ordered common → working-copy operation boundary, with the full
//!    view/working-copy/row state re-validated (leased) as observed-old before
//!    any row is deleted, and re-verified afterwards;
//! 4. the remediation is journaled as an immutable [`OperationKind::Repair`]
//!    operation carrying before/after conflict-snapshot digests and a verified
//!    receipt, so an interrupted process leaves either nothing (redb rollback)
//!    or a complete, verified journal entry — never an untracked table delete;
//! 5. repeating the operation after it succeeded is a read-only no-op.
//!
//! Only the `CONFLICTS` rows for the named inodes change. Working-tree bytes,
//! inode modes and identity, Git refs/index, view membership, and every other
//! conflict row are untouched.

use super::*;
use atomic_core::operation::{
    ActorRef, EffectReceiptKind, Operation, OperationKind, OperationPayload, OperationScope,
    RepoStateDelta, RepoStateRef, ViewStateRef,
};
use atomic_core::pristine::{
    GraphVisibilityClosure, MutTxnT, OperationMutTxnT, ReadTxn, StoredConflict,
    StoredConflictKind, ViewState,
};
use atomic_core::OperationId;

const CONFLICT_RECONCILE_ACTOR: &str = "repository-stale-conflict-reconcile";

/// How one explicitly named path's persisted conflict state was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleConflictDisposition {
    /// No persisted conflict rows. Nothing to clear (idempotent no-op).
    AlreadyClean,
    /// The rows are verified stale and may be cleared.
    Stale,
    /// The path has no tracked inode/position on the view.
    NotTracked,
    /// A real graph conflict still renders for the path.
    GenuineConflict,
    /// The canonical render does not equal the working-tree bytes (a pending
    /// resolution the normal record path owns).
    ContentDiverged,
    /// The persisted rows include a name conflict, which this path never clears.
    UnsupportedKind,
}

/// One explicit path's classification within a [`StaleConflictReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleConflictPath {
    /// Repository-relative path as named by the caller (normalized).
    pub path: String,
    /// Tracked inode, when the path resolved to one.
    pub inode: Option<Inode>,
    /// Classification of the persisted rows.
    pub disposition: StaleConflictDisposition,
    /// The persisted rows observed for the path, when any.
    pub rows: Vec<StoredConflict>,
    /// Human-readable explanation for a non-cleanable disposition.
    pub reason: Option<String>,
    /// Canonical digest of the bytes the view's graph renders for the path,
    /// set only when the path is verified stale. The write phase re-reads the
    /// working-tree bytes under the operation leases and refuses when this
    /// digest no longer matches, so a working-tree edit that races the read
    /// phase can never be cleared as if it were metadata.
    pub render_digest: Option<Hash>,
}

/// Read-only classification of the explicit paths for a working copy's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleConflictReport {
    /// Physical working copy inspected.
    pub working_copy: WorkingCopyId,
    /// Name of the working copy's desired view.
    pub view: String,
    /// Repository-local id of that view.
    pub view_id: u64,
    /// State of that view at inspection time.
    pub view_state: Merkle,
    /// One entry per normalized explicit path, sorted by path.
    pub paths: Vec<StaleConflictPath>,
}

impl StaleConflictReport {
    /// Whether at least one explicit path is verified stale.
    pub fn has_stale(&self) -> bool {
        self.paths
            .iter()
            .any(|entry| entry.disposition == StaleConflictDisposition::Stale)
    }

    /// The verified-stale paths, sorted (the paths are already sorted).
    pub fn stale_paths(&self) -> Vec<&str> {
        self.paths
            .iter()
            .filter(|entry| entry.disposition == StaleConflictDisposition::Stale)
            .map(|entry| entry.path.as_str())
            .collect()
    }

    /// Total persisted conflict rows across the verified-stale paths.
    pub fn stale_row_count(&self) -> usize {
        self.paths
            .iter()
            .filter(|entry| entry.disposition == StaleConflictDisposition::Stale)
            .map(|entry| entry.rows.len())
            .sum()
    }
}

/// Outcome of a scoped stale-conflict reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictReconcileOutcome {
    /// Immutable operation identity when rows were cleared.
    pub operation: Option<OperationId>,
    /// View whose conflict table was reconciled.
    pub view: String,
    /// Paths whose rows were cleared, sorted.
    pub cleared_paths: Vec<String>,
    /// Number of persisted conflict rows deleted.
    pub cleared_rows: usize,
    /// Paths explicitly named but not cleared, with why.
    pub skipped: Vec<(String, StaleConflictDisposition)>,
}

impl ConflictReconcileOutcome {
    /// Whether this run changed nothing (all paths already clean).
    pub fn is_noop(&self) -> bool {
        self.operation.is_none()
    }
}

impl Repository {
    /// Classify the explicit paths' persisted conflict rows read-only.
    ///
    /// Performs no recovery, locking, or mutation. Use this for `--dry-run` and
    /// for preflight display.
    ///
    /// # Errors
    ///
    /// Refuses an empty path set, an unknown working copy, or a working copy
    /// whose desired view is missing. Storage and render failures propagate.
    pub fn inspect_stale_conflicts(
        &self,
        working_copy: WorkingCopyId,
        paths: &[String],
    ) -> Result<StaleConflictReport, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let explicit = normalize_explicit_paths(paths)?;
        let view_name = self.desired_view_name(working_copy)?;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let view = txn
            .get_view(&view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;
        self.classify_stale_conflicts(&txn, &view, working_copy, &explicit)
    }

    /// Clear the verified-stale persisted conflict rows for `paths` on the
    /// working copy's desired view, through a journaled, leased operation.
    ///
    /// A path whose render still emits a real graph conflict refuses the whole
    /// call: a genuine conflict is never silently discarded. Paths that are
    /// already clean, untracked, diverged, or name-conflicted are reported in
    /// [`ConflictReconcileOutcome::skipped`] and left untouched.
    ///
    /// The view state, working-copy record, and every leased conflict row are
    /// re-read under the write transaction and must equal what the read phase
    /// observed; any third value refuses without mutating anything.
    ///
    /// # Errors
    ///
    /// Refuses an empty path set, an unknown working copy, a missing desired
    /// view, a genuine graph conflict among the named paths, or a lease that
    /// changed after the read phase.
    pub fn reconcile_stale_conflicts(
        &self,
        working_copy: WorkingCopyId,
        paths: &[String],
    ) -> Result<ConflictReconcileOutcome, RepositoryError> {
        self.reconcile_stale_conflicts_impl(working_copy, paths, false)
    }

    /// Shared implementation behind [`Self::reconcile_stale_conflicts`] and the
    /// narrow metadata-only record route.
    ///
    /// When `require_all_metadata_only` is set, the call clears *nothing*
    /// unless every named path is either verified stale or a tracked path with
    /// no persisted rows; any other disposition (untracked, diverged, name
    /// conflict, or no position) makes the whole call a read-only no-op. This
    /// is the all-or-nothing guard the explicit
    /// `record --allow-conflict-markers <paths>` route relies on, revalidated
    /// inside the same lease-protected transaction that performs the delete.
    pub(crate) fn reconcile_stale_conflicts_impl(
        &self,
        working_copy: WorkingCopyId,
        paths: &[String],
        require_all_metadata_only: bool,
    ) -> Result<ConflictReconcileOutcome, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let explicit = normalize_explicit_paths(paths)?;
        let view_name = self.desired_view_name(working_copy)?;

        // Serialize against every other operation before observing anything,
        // and refuse Git-owned state before any lock or mutation: the cleanup
        // never writes refs or working-tree bytes, but it must not run against
        // a Git transaction the bridge does not model.
        let common = self.try_lock_common_operation()?;
        ensure_git_quiescent_for_stale_cleanup(self.root())?;

        // Read phase: classify and capture the expected-old leases.
        let (report, before_digest, working_copy_before) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let view = txn
                .get_view(&view_name)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.clone(),
                })?;
            let report = self.classify_stale_conflicts(&txn, &view, working_copy, &explicit)?;
            let snapshot = txn
                .snapshot_conflicts()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let record = txn
                .get_working_copy(working_copy)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
            (report, conflict_snapshot_digest(&snapshot), record)
        };

        // A genuine conflict among the named paths refuses the whole call.
        if let Some(blocker) = report
            .paths
            .iter()
            .find(|entry| entry.disposition == StaleConflictDisposition::GenuineConflict)
        {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "refusing to clear conflict metadata for '{}': the canonical render still \
                     emits a genuine graph conflict; resolve it before recording",
                    blocker.path
                ),
            });
        }

        let stale: Vec<&StaleConflictPath> = report
            .paths
            .iter()
            .filter(|entry| entry.disposition == StaleConflictDisposition::Stale)
            .collect();
        let skipped: Vec<(String, StaleConflictDisposition)> = report
            .paths
            .iter()
            .filter(|entry| entry.disposition != StaleConflictDisposition::Stale)
            .map(|entry| (entry.path.clone(), entry.disposition))
            .collect();

        // All-or-nothing revalidation: when the caller requires a pure
        // metadata-only plan, a named path that is neither verified stale nor a
        // tracked path with no rows aborts the whole cleanup without mutating
        // anything. A race that makes a path non-cleanable therefore refuses
        // the batch rather than clearing the remaining stale rows.
        if require_all_metadata_only && !metadata_only_report(&report) {
            return Ok(ConflictReconcileOutcome {
                operation: None,
                view: report.view,
                cleared_paths: Vec::new(),
                cleared_rows: 0,
                skipped,
            });
        }

        if stale.is_empty() {
            return Ok(ConflictReconcileOutcome {
                operation: None,
                view: report.view,
                cleared_paths: Vec::new(),
                cleared_rows: 0,
                skipped,
            });
        }

        let mut txn = common.begin_write_immediate()?;

        // Lease re-validation: the view, the working-copy record, and the full
        // conflict snapshot must be exactly what the read phase observed.
        let view_now = txn
            .get_view(&view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;
        if view_now.id != report.view_id || view_now.state != report.view_state {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "refusing stale-conflict cleanup: view '{view_name}' changed after the \
                     read phase (expected id {} state {}, found id {} state {})",
                    report.view_id,
                    report.view_state.to_base32(),
                    view_now.id,
                    view_now.state.to_base32()
                ),
            });
        }
        let working_copy_now = txn
            .get_working_copy(working_copy)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
        if working_copy_now != working_copy_before {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "refusing stale-conflict cleanup: working copy {working_copy} record changed \
                     after the read phase"
                ),
            });
        }
        let snapshot_before = txn
            .snapshot_conflicts()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        if conflict_snapshot_digest(&snapshot_before) != before_digest {
            return Err(RepositoryError::InvalidOperation {
                message: "refusing stale-conflict cleanup: the persisted conflict table changed \
                          after the read phase"
                    .to_string(),
            });
        }
        for entry in &stale {
            let inode = entry.inode.ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "refusing stale-conflict cleanup for '{}': no tracked inode",
                    entry.path
                ),
            })?;
            let observed = txn
                .get_conflicts(view_now.id, inode.get())
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            if !conflict_records_equal(&observed, &entry.rows) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "refusing stale-conflict cleanup for '{}': persisted rows changed after \
                         the read phase",
                        entry.path
                    ),
                });
            }
        }

        // Content re-validation under the lease: the canonical render is a pure
        // function of the (leased) view graph, so a matching render digest
        // proves the working-tree bytes are still exactly what the read phase
        // classified as stale. A working-tree edit that raced the read phase
        // refuses here, before any row is deleted.
        for entry in &stale {
            let Some(expected) = entry.render_digest else {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "refusing stale-conflict cleanup for '{}': no verified render digest",
                        entry.path
                    ),
                });
            };
            let disk = std::fs::read(self.root.join(&entry.path)).map_err(|error| {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "refusing stale-conflict cleanup for '{}': cannot re-read the \
                         working-tree file under the lease: {error}",
                        entry.path
                    ),
                }
            })?;
            if Hash::of(&disk) != expected {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "refusing stale-conflict cleanup for '{}': the working-tree bytes changed \
                         after the read phase",
                        entry.path
                    ),
                });
            }
        }

        // Git quiescence is a precondition, not a wait: re-check it under the
        // retained common lease immediately before the delete so a lock or
        // sequence marker that appeared after preflight refuses here.
        ensure_git_quiescent_for_stale_cleanup(self.root())?;

        // Mutation: delete exactly the leased rows.
        let mut cleared_paths = Vec::with_capacity(stale.len());
        let mut cleared_rows = 0usize;
        for entry in &stale {
            let inode = entry.inode.expect("stale entry always carries an inode");
            cleared_rows += entry.rows.len();
            txn.del_conflicts(view_now.id, inode.get())
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            cleared_paths.push(entry.path.clone());
        }

        // Post-write verification: every target row is gone and every other
        // row is byte-identical to the before snapshot.
        for entry in &stale {
            let inode = entry.inode.expect("stale entry always carries an inode");
            let remaining = txn
                .get_conflicts(view_now.id, inode.get())
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            if !remaining.is_empty() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "stale-conflict cleanup failed post-write verification: '{}' still has \
                         {} persisted row(s)",
                        entry.path,
                        remaining.len()
                    ),
                });
            }
        }
        let snapshot_after = txn
            .snapshot_conflicts()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let cleared_inodes: std::collections::HashSet<(u64, Inode)> = stale
            .iter()
            .map(|entry| {
                (view_now.id, entry.inode.expect("stale entry always carries an inode"))
            })
            .collect();
        let expected_after: Vec<(u64, Inode, Vec<StoredConflict>)> = snapshot_before
            .iter()
            .filter(|(view_id, inode, _)| !cleared_inodes.contains(&(*view_id, *inode)))
            .cloned()
            .collect();
        if !conflict_snapshots_equal(&snapshot_after, &expected_after) {
            return Err(RepositoryError::InvalidOperation {
                message: "stale-conflict cleanup failed post-write verification: an unrelated \
                          conflict row changed"
                    .to_string(),
            });
        }

        // Journal the remediation with before/after snapshot digests and a
        // same-transaction verified receipt.
        let parent = self.ensure_repository_anchor_in_txn(&mut txn, &RepoStateRef::EMPTY)?;
        let after_digest = conflict_snapshot_digest(&snapshot_after);
        let view_ref = || ViewStateRef {
            name: view_name.clone(),
            state: view_now.state,
            set_id: None,
        };
        let operation = Operation::new(OperationPayload {
            parents: vec![parent],
            kind: OperationKind::Repair,
            relation: None,
            working_copy: None,
            before: RepoStateRef {
                view: Some(view_ref()),
                working_copy: None,
                git: None,
            },
            delta: RepoStateDelta {
                after: RepoStateRef {
                    view: Some(view_ref()),
                    working_copy: None,
                    git: None,
                },
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: vec![before_digest, after_digest],
            actor: ActorRef::System {
                name: CONFLICT_RECONCILE_ACTOR.to_string(),
            },
            timestamp_ms: super::operation::current_operation_timestamp_ms(),
            lossy: Vec::new(),
        })
        .map_err(super::operation::codec_error)?;
        txn.put_operation(&operation)
            .map_err(super::operation::pristine_error)?;
        txn.compare_and_set_operation_heads(
            OperationScope::Repository,
            &[parent],
            &[operation.id()],
        )
        .map_err(super::operation::pristine_error)?;
        let verified = super::operation::deterministic_effect_receipt(
            &operation,
            None,
            EffectReceiptKind::Verified,
            None,
            None,
        )?;
        txn.append_effect_receipt(&verified)
            .map_err(super::operation::pristine_error)?;
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;

        Ok(ConflictReconcileOutcome {
            operation: Some(operation.id()),
            view: report.view,
            cleared_paths,
            cleared_rows,
            skipped,
        })
    }

    /// Reconcile the explicitly named paths for the record path.
    ///
    /// Returns `Ok(None)` when no path is verified stale (or the report shows
    /// only paths the normal record flow owns). Refuses a genuine graph
    /// conflict among the named paths, so `--allow-conflict-markers` can never
    /// bake an unresolved conflict into history.
    pub(crate) fn reconcile_explicit_conflicts(
        &self,
        working_copy: WorkingCopyId,
        paths: &[String],
    ) -> Result<Option<ConflictReconcileOutcome>, RepositoryError> {
        let report = self.inspect_stale_conflicts(working_copy, paths)?;
        if report.paths.iter().any(|entry| {
            entry.disposition == StaleConflictDisposition::GenuineConflict
        }) {
            let blocker = report
                .paths
                .iter()
                .find(|entry| entry.disposition == StaleConflictDisposition::GenuineConflict)
                .expect("checked above");
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "refusing to record '{}' with --allow-conflict-markers: the canonical render \
                     still emits a genuine graph conflict",
                    blocker.path
                ),
            });
        }
        let outcome = self.reconcile_stale_conflicts(working_copy, paths)?;
        if outcome.is_noop() {
            Ok(None)
        } else {
            Ok(Some(outcome))
        }
    }

    /// Proven-metadata-only route for `record --allow-conflict-markers <paths>`
    /// in a workspace that is not anchored to a Git baseline.
    ///
    /// The ordinary record boundary refuses an unanchored workspace before the
    /// record body runs. An administrative stale-conflict cleanup needs no
    /// anchored baseline: it neither records content nor touches Git, the
    /// working tree, the view, or refs. This route therefore runs the same
    /// leased reconcile as the record path, but only when the caller's read-only
    /// preflight proves *every* named path is metadata-only:
    ///
    /// - `Stale` (canonical render emits no conflict region, render bytes equal
    ///   the working-tree bytes), or
    /// - a tracked path with no persisted conflict rows.
    ///
    /// Any other disposition — untracked, content-diverged, name-conflicted, no
    /// graph position, or a still-rendering genuine conflict — returns
    /// `Ok(None)` and the caller must fall back to the ordinary guard, which
    /// clears nothing.
    ///
    /// Git-owned state (an in-progress operation, unmerged index stages, an
    /// index lock, or administrative ref locks) refuses through
    /// [`Self::reconcile_stale_conflicts_impl`] with a typed error; it is never
    /// skipped. Read-only inspection failures also fall back to the ordinary
    /// guard rather than fabricating a baseline.
    ///
    /// # Errors
    ///
    /// Propagates the lease/precondition refusals of the reconcile it delegates
    /// to; storage and render failures during preflight become `Ok(None)` so the
    /// ordinary boundary owns the refusal.
    pub fn record_metadata_only_conflict_cleanup(
        &self,
        working_copy: WorkingCopyId,
        paths: &[String],
    ) -> Result<Option<ConflictReconcileOutcome>, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let explicit = normalize_explicit_paths(paths)?;
        let report = match self.inspect_stale_conflicts(working_copy, &explicit) {
            Ok(report) => report,
            // Cannot prove a metadata-only plan: defer to the ordinary guard.
            Err(error) => {
                log::debug!(
                    "record: metadata-only conflict cleanup preflight could not classify \
                     '{}': {error}",
                    explicit.join(", ")
                );
                return Ok(None);
            }
        };
        if !metadata_only_report(&report) || !report.has_stale() {
            return Ok(None);
        }
        let outcome =
            self.reconcile_stale_conflicts_impl(working_copy, &explicit, true)?;
        if outcome.is_noop() {
            Ok(None)
        } else {
            Ok(Some(outcome))
        }
    }

    /// Classify each explicit path against the view's persisted rows and the
    /// canonical render. Read-only.
    fn classify_stale_conflicts(
        &self,
        txn: &ReadTxn,
        view: &ViewState,
        working_copy: WorkingCopyId,
        explicit: &[String],
    ) -> Result<StaleConflictReport, RepositoryError> {
        let visibility = graph_visibility_closure(txn, view)?;
        let inode_graph_table = txn
            .open_inode_graph_table()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let mut external_hashes = std::collections::HashMap::new();
        for node_id in visibility.iter_dependency_first().copied() {
            if node_id.is_root() {
                continue;
            }
            let hash = txn
                .get_external(node_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "visible change {} has no external hash",
                        node_id.get()
                    ))
                })?;
            external_hashes.insert(node_id, hash);
        }

        let mut paths = Vec::with_capacity(explicit.len());
        for path in explicit {
            paths.push(self.classify_stale_path(
                txn,
                view,
                &visibility,
                &inode_graph_table,
                &external_hashes,
                path,
            )?);
        }

        Ok(StaleConflictReport {
            working_copy,
            view: view.name.clone(),
            view_id: view.id,
            view_state: view.state,
            paths,
        })
    }

    fn classify_stale_path(
        &self,
        txn: &ReadTxn,
        view: &ViewState,
        visibility: &GraphVisibilityClosure,
        inode_graph_table: &redb::ReadOnlyMultimapTable<&'static [u8; 32], &'static [u8; 24]>,
        external_hashes: &std::collections::HashMap<NodeId, Hash>,
        path: &str,
    ) -> Result<StaleConflictPath, RepositoryError> {
        let inode = txn
            .get_inode(path)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let Some(inode) = inode else {
            return Ok(StaleConflictPath {
                path: path.to_string(),
                inode: None,
                disposition: StaleConflictDisposition::AlreadyClean,
                rows: Vec::new(),
                reason: Some("path is not tracked on this view".to_string()),
                render_digest: None,
            });
        };
        let rows = txn
            .get_conflicts(view.id, inode.get())
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        if rows
            .iter()
            .any(|record| record.kind == StoredConflictKind::Name)
        {
            return Ok(StaleConflictPath {
                path: path.to_string(),
                inode: Some(inode),
                disposition: StaleConflictDisposition::UnsupportedKind,
                rows,
                reason: Some("persisted name conflict; resolve it through the record path".to_string()),
                render_digest: None,
            });
        }
        let Some(position) = txn
            .inode_position(inode)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        else {
            return Ok(StaleConflictPath {
                path: path.to_string(),
                inode: Some(inode),
                disposition: StaleConflictDisposition::NotTracked,
                rows,
                reason: Some("tracked inode has no graph position".to_string()),
                render_digest: None,
            });
        };

        let (rendered, regions) = super::materialize::capture_file_conflict_bytes(
            txn,
            &self.change_store,
            inode_graph_table,
            visibility,
            external_hashes,
            path,
            inode,
            position,
        )
        .map_err(|message| RepositoryError::InvalidOperation {
            message: format!("cannot render '{path}' to classify its conflict state: {message}"),
        })?;
        if !regions.is_empty() {
            return Ok(StaleConflictPath {
                path: path.to_string(),
                inode: Some(inode),
                disposition: StaleConflictDisposition::GenuineConflict,
                rows,
                reason: Some("the canonical render still emits a conflict region".to_string()),
                render_digest: None,
            });
        }
        let disk = match std::fs::read(self.root.join(path)) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(StaleConflictPath {
                    path: path.to_string(),
                    inode: Some(inode),
                    disposition: StaleConflictDisposition::ContentDiverged,
                    rows,
                    reason: Some(format!(
                        "cannot read the working-tree file to compare with the render: {error}"
                    )),
                    render_digest: None,
                });
            }
        };
        if rendered != disk {
            return Ok(StaleConflictPath {
                path: path.to_string(),
                inode: Some(inode),
                disposition: StaleConflictDisposition::ContentDiverged,
                rows,
                reason: Some(
                    "the canonical render does not equal the working-tree bytes".to_string(),
                ),
                render_digest: None,
            });
        }

        // A tracked path with no persisted rows is clean *only when its
        // working-tree bytes equal the canonical render*. Rendering here (not
        // just for paths that already carry rows) is what proves a named path
        // has no content delta, so a mixed metadata-plus-content scope can
        // never be mistaken for a metadata-only cleanup.
        if rows.is_empty() {
            return Ok(StaleConflictPath {
                path: path.to_string(),
                inode: Some(inode),
                disposition: StaleConflictDisposition::AlreadyClean,
                rows,
                reason: None,
                render_digest: None,
            });
        }

        Ok(StaleConflictPath {
            path: path.to_string(),
            inode: Some(inode),
            disposition: StaleConflictDisposition::Stale,
            rows,
            reason: None,
            render_digest: Some(Hash::of(&rendered)),
        })
    }
}

/// Whether every named path is a pure metadata-only cleanup target: verified
/// stale, or a tracked path with no persisted rows. Untracked paths, diverged
/// content, name conflicts, and paths with no graph position are excluded.
fn metadata_only_report(report: &StaleConflictReport) -> bool {
    report.paths.iter().all(|entry| {
        entry.disposition == StaleConflictDisposition::Stale
            || (entry.disposition == StaleConflictDisposition::AlreadyClean
                && entry.inode.is_some())
    })
}

/// Refuse stale-conflict cleanup while Git owns a transaction the bridge does
/// not model.
///
/// The cleanup writes no refs and no working-tree bytes, but it must not run
/// while an in-progress Git operation, unmerged index stages, an index lock, or
/// an administrative ref lock exists. The enumeration is fail-closed.
fn ensure_git_quiescent_for_stale_cleanup(root: &Path) -> Result<(), RepositoryError> {
    let observation = observe_git_metadata(root).map_err(|error| {
        RepositoryError::InvalidRepository {
            reason: format!("cannot observe Git state before stale-conflict cleanup: {error}"),
        }
    })?;
    if let WorkspaceGitObservation::Repository(git) = &observation {
        let conflict_stages = git.conflict_stages();
        if git.operation.is_in_progress() || !conflict_stages.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "refusing stale-conflict cleanup: Git owns an in-progress operation ({} \
                     sequence marker(s), {} unmerged index stage(s))",
                    git.operation.present_markers().len(),
                    conflict_stages.len()
                ),
            });
        }
    }
    if let GitQuiescence::Busy { reason, detail } = GitQuiescence::evaluate(&observation) {
        return Err(RepositoryError::InvalidOperation {
            message: format!("refusing stale-conflict cleanup: Git is busy ({reason}): {detail}"),
        });
    }
    Ok(())
}

fn normalize_explicit_paths(paths: &[String]) -> Result<Vec<String>, RepositoryError> {
    if paths.is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: "scoped conflict reconciliation requires at least one explicit path"
                .to_string(),
        });
    }
    let mut normalized: Vec<String> = Vec::with_capacity(paths.len());
    for raw in paths {
        let value = normalize_path(Path::new(raw));
        if value.is_empty() || value == "." {
            return Err(RepositoryError::InvalidOperation {
                message: format!("invalid explicit path '{raw}' for conflict reconciliation"),
            });
        }
        if !normalized.iter().any(|existing| existing == &value) {
            normalized.push(value);
        }
    }
    normalized.sort();
    Ok(normalized)
}

/// Whether two persisted-row vectors describe the same conflicts.
fn conflict_records_equal(left: &[StoredConflict], right: &[StoredConflict]) -> bool {
    fn sort_key(record: &StoredConflict) -> (String, String, Option<u32>, Vec<String>) {
        (
            record.path.clone(),
            record.kind.to_string(),
            record.line,
            record.sides.clone(),
        )
    }
    let mut left: Vec<_> = left.iter().cloned().collect();
    let mut right: Vec<_> = right.iter().cloned().collect();
    left.sort_by_key(sort_key);
    right.sort_by_key(sort_key);
    left == right
}

fn conflict_snapshots_equal(
    left: &[(u64, Inode, Vec<StoredConflict>)],
    right: &[(u64, Inode, Vec<StoredConflict>)],
) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .all(|(a, b)| a.0 == b.0 && a.1 == b.1 && conflict_records_equal(&a.2, &b.2))
}

/// Canonical digest over a complete `CONFLICTS` snapshot. Stable under row and
/// record ordering so the journal evidence names the exact before/after state.
fn conflict_snapshot_digest(snapshot: &[(u64, Inode, Vec<StoredConflict>)]) -> Hash {
    fn push_str(buffer: &mut Vec<u8>, value: &str) {
        buffer.extend_from_slice(&(value.len() as u32).to_le_bytes());
        buffer.extend_from_slice(value.as_bytes());
    }

    let mut buffer = Vec::new();
    buffer.extend_from_slice(b"atomic:conflict-snapshot:v1\0");
    let mut rows: Vec<&(u64, Inode, Vec<StoredConflict>)> = snapshot.iter().collect();
    rows.sort_by_key(|(view_id, inode, _)| (*view_id, inode.get()));
    buffer.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (view_id, inode, records) in rows {
        buffer.extend_from_slice(&view_id.to_le_bytes());
        buffer.extend_from_slice(&inode.get().to_le_bytes());
        let mut records: Vec<&StoredConflict> = records.iter().collect();
        records.sort_by(|a, b| {
            (a.path.as_str(), a.kind.to_string())
                .cmp(&(b.path.as_str(), b.kind.to_string()))
                .then(a.line.cmp(&b.line))
                .then(a.sides.cmp(&b.sides))
        });
        buffer.extend_from_slice(&(records.len() as u32).to_le_bytes());
        for record in records {
            push_str(&mut buffer, &record.path);
            push_str(&mut buffer, &record.kind.to_string());
            match record.line {
                Some(line) => {
                    buffer.push(1);
                    buffer.extend_from_slice(&line.to_le_bytes());
                }
                None => buffer.push(0),
            }
            buffer.extend_from_slice(&(record.sides.len() as u32).to_le_bytes());
            for side in &record.sides {
                push_str(&mut buffer, side);
            }
        }
    }
    Hash::of(&buffer)
}
