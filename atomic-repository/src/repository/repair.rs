//! Verification and atomic repair of native derived repository indexes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use atomic_core::change::GraphOp;
use atomic_core::operation::{
    ActorRef, Operation, OperationKind, OperationPayload, OperationScope, RepoStateDelta,
    RepoStateRef,
};
use atomic_core::pristine::{
    decode_path_claim_event, directory_flags, encode_path_claim_event, GraphTxnT,
    GraphVisibilityClosure, InodeGraphOps, NativeDerivedIndexes, NativeDerivedIndexesMutTxnT,
    OperationMutTxnT, PathClaimEntry, PathClaimKind, PathClaimTxnT, PristineError, StoredConflict,
    StoredConflictKind, TreeTxnT, ViewTxnT, PATH_CLAIM_EVENT_SIZE,
};
use atomic_core::types::{Inode, NodeId, Position};
use atomic_core::{Hash, OperationId};

use super::operation::{codec_error, pristine_error};
use super::*;

/// Native derived table audited by `atomic doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NativeIndex {
    PathClaims,
    Tree,
    RevTree,
    Inodes,
    RevInodes,
    Directories,
    Conflicts,
}

impl NativeIndex {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PathClaims => "PATH_CLAIMS",
            Self::Tree => "TREE",
            Self::RevTree => "REV_TREE",
            Self::Inodes => "INODES",
            Self::RevInodes => "REV_INODES",
            Self::Directories => "DIRECTORIES",
            Self::Conflicts => "CONFLICTS",
        }
    }
}

impl std::fmt::Display for NativeIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kind of mismatch between an authoritative projection and its stored cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NativeIndexProblemKind {
    Missing,
    Stale,
    Mismatched,
    Malformed,
    Unrepairable,
}

impl std::fmt::Display for NativeIndexProblemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Missing => "missing",
            Self::Stale => "stale",
            Self::Mismatched => "mismatched",
            Self::Malformed => "malformed",
            Self::Unrepairable => "unrepairable",
        };
        f.write_str(value)
    }
}

/// One deterministic native-index diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeIndexProblem {
    pub index: NativeIndex,
    pub kind: NativeIndexProblemKind,
    pub key: String,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

impl std::fmt::Display for NativeIndexProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} row '{}'", self.index, self.kind, self.key)?;
        if let Some(expected) = &self.expected {
            write!(f, " expected={expected}")?;
        }
        if let Some(actual) = &self.actual {
            write!(f, " actual={actual}")?;
        }
        Ok(())
    }
}

/// Read-only comparison of native derived indexes with graph/change authority.
#[derive(Debug, Clone, Default)]
pub struct NativeIndexReport {
    pub expected_rows: usize,
    pub actual_rows: usize,
    pub problems: Vec<NativeIndexProblem>,
}

impl NativeIndexReport {
    pub fn is_healthy(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Outcome of an all-or-nothing native-index repair.
#[derive(Debug, Clone, Default)]
pub struct NativeIndexRepairOutcome {
    pub problems_repaired: usize,
    pub rows_written: usize,
    pub already_healthy: bool,
    /// Immutable remediation operation journaling the executed repair
    /// (CB-13A R3). `None` when the index was already healthy and no repair
    /// transaction ran — a healthy check journals nothing.
    pub operation: Option<OperationId>,
}

/// Derive the graph-authoritative path-claim set: the complete closure
/// union across every view, topologically ordered change replay, the
/// recovered inode owners for every structural claimant, and the derived
/// claim events. Shared by the full native-index rebuild and the targeted
/// PATH_CLAIMS repair so both use the identical derivation contract.
#[allow(clippy::type_complexity)] // reconstructed claim index pairing
fn derive_native_path_claim_authority<T>(
    repo: &Repository,
    txn: &T,
) -> Result<(Vec<PathClaimEntry>, HashMap<Position<NodeId>, Inode>), RepositoryError>
where
    T: GraphTxnT + TreeTxnT + PathClaimTxnT + ViewTxnT + InodeGraphOps<InodeError = PristineError>,
{
    let view_snapshot = txn
        .snapshot_views()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let view_names: Vec<String> = view_snapshot.iter().map(|(name, _)| name.clone()).collect();

    let mut reachable = Vec::new();
    let mut reachable_set = HashSet::new();
    for view_name in &view_names {
        let view = txn
            .get_view(view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;
        let visibility = graph_visibility_closure(txn, &view)?;
        for change_id in visibility.iter_dependency_first().copied() {
            if !change_id.is_root() && reachable_set.insert(change_id) {
                reachable.push(change_id);
            }
        }
    }
    let mut ordered = Vec::with_capacity(reachable.len());
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for change_id in reachable {
        visit_reachable_change(
            txn,
            change_id,
            &reachable_set,
            &mut visiting,
            &mut visited,
            &mut ordered,
        )?;
    }

    let mut changes = Vec::with_capacity(ordered.len());
    let mut root_kinds = BTreeMap::<Position<NodeId>, PathClaimKind>::new();
    for change_id in ordered {
        let hash = txn
            .get_external(change_id)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "reachable change {} has no external hash",
                    change_id.get()
                ))
            })?;
        let change = repo.change_store.load_change(&hash).map_err(|error| {
            RepositoryError::Database(format!(
                "cannot load reachable change {} while rebuilding native indexes: {}",
                hash, error
            ))
        })?;
        for operation in change.hunks() {
            let root = match operation {
                GraphOp::FileAdd { add_inode, .. } => Some((
                    Position::new(change_id, add_inode.start),
                    PathClaimKind::File,
                )),
                GraphOp::DirAdd { add_inode, .. } => Some((
                    Position::new(change_id, add_inode.start),
                    PathClaimKind::Directory,
                )),
                _ => None,
            };
            if let Some((position, kind)) = root {
                if let Some(previous) = root_kinds.insert(position, kind) {
                    if previous != kind {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "graph root {} changes between file and directory",
                                position
                            ),
                        });
                    }
                }
            }
        }
        changes.push((change_id, change));
    }
    let inode_graph_keys = txn
        .snapshot_inode_graph_keys()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let existing_rev_inodes: BTreeMap<_, _> = txn
        .snapshot_rev_inodes()
        .map_err(|error| RepositoryError::Database(error.to_string()))?
        .into_iter()
        .collect();
    let mut recovered = HashMap::<Position<NodeId>, Inode>::new();
    for position in root_kinds.keys().copied() {
        let candidates: BTreeSet<Inode> = inode_graph_keys
            .iter()
            .filter(|(_, node)| {
                node.change == position.change
                    && node.start == position.pos
                    && node.end == position.pos
            })
            .map(|(inode, _)| *inode)
            .collect();
        let inode = match candidates.len() {
            1 => *candidates.iter().next().expect("one inode owner"),
            _ => match existing_rev_inodes.get(&position).copied() {
                Some(existing) if candidates.contains(&existing) => existing,
                _ => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "graph root {} has {} candidate INODE_GRAPH owners and no matching existing binding; repair is unsafe",
                            position,
                            candidates.len()
                        ),
                    });
                }
            },
        };
        let root = inode_graph_keys
            .iter()
            .find(|(candidate, node)| {
                *candidate == inode
                    && node.change == position.change
                    && node.start == position.pos
                    && node.end == position.pos
            })
            .map(|(_, node)| *node)
            .expect("candidate was derived from an exact empty root");
        if !txn
            .has_vertex(root)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "INODE_GRAPH owner {} for root {} has no canonical GRAPH vertex",
                    inode.get(),
                    position
                ),
            });
        }
        if existing_rev_inodes
            .get(&position)
            .is_some_and(|existing| *existing != inode)
        {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "graph root {} is owned by inode {} in INODE_GRAPH but inode {} in REV_INODES",
                    position,
                    inode.get(),
                    existing_rev_inodes[&position].get()
                ),
            });
        }
        recovered.insert(position, inode);
    }
    let mut claims = Vec::<PathClaimEntry>::new();
    for (change_id, change) in &changes {
        let mut change_claims =
            super::name_resolution::path_claim_events_for_change_with_prior_and_inodes(
                txn, *change_id, change, &claims, &recovered,
            )?;
        claims.append(&mut change_claims);
    }
    claims.sort_by(|left, right| (&left.path, left.event).cmp(&(&right.path, right.event)));
    claims.dedup_by(|left, right| left.path == right.path && left.event == right.event);

    for claim in &claims {
        if !recovered.contains_key(&claim.event.claim.claimant) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path claim '{}' references graph root {} with no unique inode owner",
                    claim.path, claim.event.claim.claimant
                ),
            });
        }
    }
    Ok((claims, recovered))
}

impl Repository {
    /// Compare every native derived table with a deterministic graph projection.
    pub fn verify_native_derived_indexes(&self) -> Result<NativeIndexReport, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let expected = match self.build_native_derived_indexes(&txn) {
            Ok(expected) => expected,
            Err(error) => {
                return Ok(NativeIndexReport {
                    problems: vec![NativeIndexProblem {
                        index: NativeIndex::Inodes,
                        kind: NativeIndexProblemKind::Unrepairable,
                        key: "<projection>".to_string(),
                        expected: None,
                        actual: Some(error.to_string()),
                    }],
                    ..NativeIndexReport::default()
                });
            }
        };
        compare_native_derived_indexes(&txn, &expected)
    }

    /// Atomically rebuild every native derived table from graph and change facts.
    pub fn repair_native_derived_indexes(
        &self,
    ) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        self.repair_native_derived_indexes_inner(false)
    }

    /// Targeted repair of the PATH_CLAIMS index from graph authority (review
    /// E4).
    ///
    /// The full native-index rebuild re-derives every derived table for every
    /// view, including a per-file conflict-marker content scan across every
    /// view — which on a large, long-history repository is far beyond an
    /// interactive budget. When ONLY the path-claim index is divergent (the
    /// structural PATH_CLAIMS rows a historical apply window never wrote),
    /// this repair reuses the identical graph-authoritative claim derivation
    /// and writes ONLY the missing PATH_CLAIMS rows, atomically and
    /// insert-only:
    ///
    /// - every derived claim that already exists must be byte-identical in
    ///   the table, or the repair refuses (divergence is corruption, never
    ///   silently overwritten);
    /// - every table row the derivation does NOT produce is refused (stale
    ///   rows cannot be dropped here — run the full rebuild);
    /// - only genuinely missing rows are inserted, idempotently, in one
    ///   transaction verified after the write.
    ///
    /// No change bytes, TREE/INODES bindings, graph rows, or views are
    /// touched.
    pub fn repair_path_claims_index(&self) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        use atomic_core::pristine::PathClaimMutTxnT;

        // CB-13A R3: the explicit repair is an immutable, journaled
        // remediation. It acquires the repository-common operation boundary
        // FIRST, then performs the row-lease comparison, the insert-only
        // mutation, its post-write verification, and the Repair operation
        // with its verified receipt inside ONE immediately durable
        // transaction. An interrupted process leaves either nothing (redb
        // rollback — the next run re-executes idempotently) or a complete,
        // verified journal entry. There is no untracked table mutation.
        let common = self.try_lock_common_operation()?;
        let mut txn = common.begin_write_immediate()?;
        let parent = self.ensure_repository_anchor_in_txn(
            &mut txn,
            &RepoStateRef {
                view: None,
                working_copy: None,
                git: None,
            },
        )?;

        let (expected_claims, _recovered) = derive_native_path_claim_authority(self, &*txn)?;

        // Compare the derived authority against the live table, byte-exact.
        let mut current: std::collections::BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> =
            std::collections::BTreeMap::new();
        for entry in txn
            .iter_path_claims()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let encoded = encode_path_claim_event(&entry.event);
            let rows = current.entry(entry.path).or_default();
            if !rows.contains(&encoded) {
                rows.push(encoded);
            }
        }
        let mut derived: std::collections::BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> =
            std::collections::BTreeMap::new();
        for entry in &expected_claims {
            let encoded = encode_path_claim_event(&entry.event);
            derived.entry(entry.path.clone()).or_default().push(encoded);
        }
        // Rows the live table holds that the derivation does not produce are
        // stale: a targeted repair cannot reconcile them and fails closed.
        // This comparison is row-level (review F4): an extra stale event on a
        // path the derivation also produces must refuse exactly like rows for
        // a path the derivation does not know at all. A path-keyed check
        // alone lets `current[path] = derived[path] + stale event` reach the
        // `already_healthy` return whenever no derived row is missing, so the
        // repair would call an index that still disagrees with graph authority
        // healthy.
        for (path, rows) in &current {
            let produced = derived.get(path);
            for encoded in rows {
                if produced.is_none_or(|rows| !rows.contains(encoded)) {
                    // The row lease rejected the observed table: refuse
                    // without mutating anything and without journaling.
                    // Nothing was written, so there is no effect to report.
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "path-claim repair refused: the live PATH_CLAIMS index holds event(s) \
                             for '{path}' that the graph derivation does not produce; a targeted \
                             repair cannot safely reconcile stale rows — run the full \
                             native-index rebuild"
                        ),
                    });
                }
            }
        }
        // Per-path comparison: only ADD rows the live table is missing; any
        // divergence on an existing row is corruption and refuses.
        let mut missing_rows: Vec<(String, [u8; PATH_CLAIM_EVENT_SIZE])> = Vec::new();
        for (path, rows) in &derived {
            // A derived path absent from the live index means every one of
            // its rows is missing — the structural claim a historical apply
            // window never wrote (review E4).
            let empty: Vec<[u8; PATH_CLAIM_EVENT_SIZE]> = Vec::new();
            let existing = current.get(path).unwrap_or(&empty);
            for encoded in rows {
                if !existing.contains(encoded) {
                    missing_rows.push((path.clone(), *encoded));
                }
            }
        }

        if missing_rows.is_empty() {
            if std::env::var("ATOMIC_DEBUG_REPAIR_PHASES").is_ok() {
                eprintln!(
                    "REPAIR_DERIVED_COUNT {} current_paths {} derived_paths {} sample {}",
                    expected_claims.len(),
                    current.len(),
                    derived.len(),
                    derived
                        .keys()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                for want in [
                    "atomic-repository/src/repository/synthesis.rs",
                    "atomic-cli/tests/git_import_cb9b_test.rs",
                ] {
                    let d = derived.get(want).map(|r| r.len());
                    let c = current.get(want).map(|r| r.len());
                    eprintln!("REPAIR_PATH {want} derived={d:?} current={c:?}");
                }
            }
            txn.abort()?;
            return Ok(NativeIndexRepairOutcome {
                already_healthy: true,
                rows_written: 0,
                ..NativeIndexRepairOutcome::default()
            });
        }

        let missing_count = missing_rows.len();
        // CB-13A follow-up R3: the plan digest is RECONSTRUCTIBLE — it
        // covers the complete old table (every live row byte-exact) plus
        // the missing rows the repair will insert, not merely a row count.
        // The undo path restores `old_rows` under fresh leases.
        let plan_digest = Hash::of(&canonical_tagged_bytes(
            b"repair:path-claims:plan",
            &[
                canonical_path_claim_rows_bytes(&current),
                canonical_missing_rows_bytes(&missing_rows),
            ]
            .concat(),
        ));
        let old_rows: BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> = current.clone();
        for (path, event_bytes) in &missing_rows {
            let event = decode_path_claim_event(event_bytes)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            txn.put_path_claim(path, &event)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        }
        // Post-write verification: every derived claim must now be present
        // with identical bytes.
        let mut after: std::collections::BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> =
            std::collections::BTreeMap::new();
        for entry in txn
            .iter_path_claims()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let encoded = encode_path_claim_event(&entry.event);
            let rows = after.entry(entry.path).or_default();
            if !rows.contains(&encoded) {
                rows.push(encoded);
            }
        }
        for (path, rows) in &derived {
            let mut sorted_rows = rows.clone();
            sorted_rows.sort();
            match after.get(path) {
                Some(existing)
                    if existing.len() == sorted_rows.len()
                        && sorted_rows.iter().all(|r| existing.contains(r)) => {}
                _ => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "path-claim repair failed post-write verification for '{path}':                              the written index does not match the graph derivation"
                        ),
                    });
                }
            }
        }
        // Journal the executed remediation with before-plan and post-write
        // evidence digests, then verify it — all inside the repair
        // transaction, so the journal entry exists only when the mutation it
        // describes is durable.
        let after_digest = Hash::of(&canonical_tagged_bytes(
            b"repair:path-claims:after",
            &canonical_path_claim_rows_bytes(&after),
        ));
        let operation = Operation::new(OperationPayload {
            parents: vec![parent],
            kind: OperationKind::Repair,
            relation: None,
            working_copy: None,
            before: RepoStateRef {
                view: None,
                working_copy: None,
                git: None,
            },
            delta: RepoStateDelta {
                after: RepoStateRef {
                    view: None,
                    working_copy: None,
                    git: None,
                },
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: vec![plan_digest, after_digest],
            actor: ActorRef::System {
                name: "repository-native-index-repair".to_string(),
            },
            timestamp_ms: super::operation::current_operation_timestamp_ms(),
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        txn.put_operation(&operation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(
            OperationScope::Repository,
            &[parent],
            &[operation.id()],
        )
        .map_err(pristine_error)?;
        let verified = super::operation::deterministic_effect_receipt(
            &operation,
            None,
            atomic_core::operation::EffectReceiptKind::Verified,
            None,
            None,
        )?;
        txn.append_effect_receipt(&verified)
            .map_err(pristine_error)?;
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        self.store_path_claims_repair_inverse(&operation, &old_rows)?;
        Ok(NativeIndexRepairOutcome {
            problems_repaired: 1,
            rows_written: missing_count,
            already_healthy: false,
            operation: Some(operation.id()),
        })
    }

    /// CB-13A R3: rebuild REV_TREE as the exact inverse of TREE inside one
    /// immediately durable, journaled transaction (the same contract as the
    /// targeted path-claim repair). History could re-bind a path without
    /// cleaning the previous inode's reverse row; every later tree write
    /// validates the bijection and refuses, so the stale rows block all
    /// TREE-mutating operations until this repair runs. TREE is authoritative
    /// for the binding; the repair never touches forward rows and fails
    /// closed if the post-write state is still not an exact bijection.
    pub fn repair_tree_bijection(&self) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        let common = self.try_lock_common_operation()?;
        let mut txn = common.begin_write_immediate()?;
        let parent = self.ensure_repository_anchor_in_txn(
            &mut txn,
            &RepoStateRef {
                view: None,
                working_copy: None,
                git: None,
            },
        )?;

        // Snapshot the live rows for the plan and after digests.
        let forward: BTreeMap<String, u64> = txn
            .iter_tree()
            .map_err(pristine_error)?
            .filter_map(|row| row.ok())
            .map(|(path, inode)| (path, inode.get()))
            .collect();
        let reverse: BTreeMap<u64, String> = txn
            .iter_rev_tree_pairs()
            .map_err(pristine_error)?
            .into_iter()
            .map(|(inode, path)| (inode.get(), path))
            .collect();
        let stale: Vec<(u64, String)> = reverse
            .iter()
            .filter(|(inode, path)| forward.get(path.as_str()).copied() != Some(**inode))
            .map(|(inode, path)| (*inode, path.clone()))
            .collect();
        let missing: Vec<(String, u64)> = forward
            .iter()
            .filter(|(path, inode)| reverse.get(*inode).map(String::as_str) != Some(path.as_str()))
            .map(|(path, inode)| (path.clone(), *inode))
            .collect();
        if stale.is_empty() && missing.is_empty() {
            txn.abort().map_err(pristine_error)?;
            return Ok(NativeIndexRepairOutcome {
                already_healthy: true,
                rows_written: 0,
                ..NativeIndexRepairOutcome::default()
            });
        }

        let plan_digest = Hash::of(&canonical_tagged_bytes(
            b"repair:tree-bijection:plan",
            &canonical_tree_bijection_rows_bytes(&stale, &missing),
        ));
        let (removed, inserted) = (*txn).repair_rev_tree_bijection().map_err(pristine_error)?;

        // Post-write verification: TREE and REV_TREE must now be exact
        // one-to-one inverses.
        txn.validate_tree_bijection().map_err(pristine_error)?;

        let after_reverse: Vec<(u64, String)> = txn
            .iter_rev_tree_pairs()
            .map_err(pristine_error)?
            .into_iter()
            .map(|(inode, path)| (inode.get(), path))
            .collect();
        let after_digest = Hash::of(&canonical_tagged_bytes(
            b"repair:tree-bijection:after",
            &canonical_reverse_rows_bytes(&after_reverse),
        ));
        let operation = Operation::new(OperationPayload {
            parents: vec![parent],
            kind: OperationKind::Repair,
            relation: None,
            working_copy: None,
            before: RepoStateRef {
                view: None,
                working_copy: None,
                git: None,
            },
            delta: RepoStateDelta {
                after: RepoStateRef {
                    view: None,
                    working_copy: None,
                    git: None,
                },
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: vec![plan_digest, after_digest],
            actor: ActorRef::System {
                name: "repository-tree-bijection-repair".to_string(),
            },
            timestamp_ms: super::operation::current_operation_timestamp_ms(),
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        txn.put_operation(&operation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(
            OperationScope::Repository,
            &[parent],
            &[operation.id()],
        )
        .map_err(pristine_error)?;
        let verified = super::operation::deterministic_effect_receipt(
            &operation,
            None,
            atomic_core::operation::EffectReceiptKind::Verified,
            None,
            None,
        )?;
        txn.append_effect_receipt(&verified)
            .map_err(pristine_error)?;
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(NativeIndexRepairOutcome {
            problems_repaired: 1,
            rows_written: removed + inserted,
            already_healthy: false,
            operation: Some(operation.id()),
        })
    }

    /// Undo a previously-journaled path-claims repair (CB-13A follow-up
    /// R3): restore the complete old table captured in the repair's plan
    /// evidence under fresh leases. The CURRENT live table must be
    /// byte-exactly the repair's post-state (verified by re-deriving the
    /// graph authority and checking the current table equals derived plus
    /// nothing else); any third observed value refuses without
    /// overwriting. The undo journals an immutable inverse `Repair`
    /// operation with a verified receipt, so repeated undo is idempotent
    /// (an already-undone table refuses as a third value) and the stored
    /// inverse is the old-row set.
    ///
    /// `old_rows` come from the repair's durable evidence; the caller
    /// obtains them from the journaled operation (or `undo_last_path_claims_repair`,
    /// which reads the latest journaled repair itself).
    pub fn undo_path_claims_repair(
        &self,
        old_rows: &BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>>,
    ) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        use atomic_core::pristine::PathClaimMutTxnT;
        let common = self.try_lock_common_operation()?;
        let mut txn = common.begin_write_immediate()?;
        let parent = self.ensure_repository_anchor_in_txn(
            &mut txn,
            &RepoStateRef {
                view: None,
                working_copy: None,
                git: None,
            },
        )?;

        // Snapshot the live rows for the third-value lease check.
        let live: BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> = {
            let mut live: BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> = BTreeMap::new();
            for entry in txn.iter_path_claims().map_err(pristine_error)? {
                let encoded = encode_path_claim_event(&entry.event);
                let rows = live.entry(entry.path).or_default();
                if !rows.contains(&encoded) {
                    rows.push(encoded);
                }
            }
            live
        };

        // Third-value rejection: the live table must be exactly the graph
        // derivation (the repair's post-state). Anything else (partially
        // undone, externally mutated) refuses without overwriting.
        let derived_rows: BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> = {
            let (expected_claims, _recovered) = derive_native_path_claim_authority(self, &*txn)?;
            let mut derived: BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> = BTreeMap::new();
            for entry in &expected_claims {
                let encoded = encode_path_claim_event(&entry.event);
                let rows = derived.entry(entry.path.clone()).or_default();
                if !rows.contains(&encoded) {
                    rows.push(encoded);
                }
            }
            derived
        };
        if live != derived_rows {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path-claims undo refused: the live table is neither the repair's post-state                      nor provably recoverable ({} live path(s) vs {} derived); a third observed \
                     value must never be overwritten — inspect with 'atomic doctor check' first",
                    live.len(),
                    derived_rows.len()
                ),
            });
        }

        // Idempotence: an already-undone table (equal to old_rows) refuses
        // as a third value — there is nothing to undo.
        if live == *old_rows {
            return Err(RepositoryError::InvalidOperation {
                message:
                    "path-claims undo refused: the live table already equals the repair's                      before-state (nothing to undo)"
                        .to_string(),
            });
        }

        let before_digest = Hash::of(&canonical_tagged_bytes(
            b"repair:path-claims:undo-before",
            &canonical_path_claim_rows_bytes(&live),
        ));
        // Rewrite the live table to the captured old rows: remove rows not
        // in old_rows, insert rows missing from live.
        let mut rows_written = 0usize;
        for (path, rows) in &live {
            let Some(old) = old_rows.get(path) else {
                for encoded in rows {
                    let event = decode_path_claim_event(encoded)
                        .map_err(|error| RepositoryError::Database(error.to_string()))?;
                    atomic_core::pristine::PathClaimMutTxnT::del_path_claim(
                        &mut *txn, path, &event,
                    )
                    .map_err(pristine_error)?;
                    rows_written += 1;
                }
                continue;
            };
            for encoded in rows {
                if !old.contains(encoded) {
                    let event = decode_path_claim_event(encoded)
                        .map_err(|error| RepositoryError::Database(error.to_string()))?;
                    atomic_core::pristine::PathClaimMutTxnT::del_path_claim(
                        &mut *txn, path, &event,
                    )
                    .map_err(pristine_error)?;
                    rows_written += 1;
                }
            }
        }
        for (path, rows) in old_rows {
            let live_rows = live.get(path);
            for encoded in rows {
                if live_rows
                    .map(|live| !live.contains(encoded))
                    .unwrap_or(true)
                {
                    let event = decode_path_claim_event(encoded)
                        .map_err(|error| RepositoryError::Database(error.to_string()))?;
                    txn.put_path_claim(path, &event).map_err(pristine_error)?;
                    rows_written += 1;
                }
            }
        }

        // Post-write verification: the table must now byte-equal old_rows.
        let after: BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> = {
            let mut after: BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> = BTreeMap::new();
            for entry in txn.iter_path_claims().map_err(pristine_error)? {
                let encoded = encode_path_claim_event(&entry.event);
                let rows = after.entry(entry.path).or_default();
                if !rows.contains(&encoded) {
                    rows.push(encoded);
                }
            }
            after
        };
        if after != *old_rows {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path-claims undo failed post-write verification: {} path(s) after the                      undo do not byte-match the stored before-state; the transaction aborts",
                    after.len()
                ),
            });
        }

        let after_digest = Hash::of(&canonical_tagged_bytes(
            b"repair:path-claims:undo-after",
            &canonical_path_claim_rows_bytes(&after),
        ));
        // The undo names the journaled repair it inverts (the latest
        // repository-scope Repair head), so the provenance chain is
        // reconstructible.
        let target_operation = {
            let log = self
                .operation_log(
                    atomic_core::operation::OperationScope::Repository,
                    None,
                    false,
                )
                .map_err(pristine_error)?;
            let OperationHeadState::Single(_head) = &log.head_state else {
                return Err(RepositoryError::InvalidOperation {
                    message: "path-claims undo refused: no operation head to name".to_string(),
                });
            };
            let Some(entry) = log.entries.iter().find(|entry| entry.is_head) else {
                return Err(RepositoryError::InvalidOperation {
                    message: "path-claims undo refused: no journaled repair to undo".to_string(),
                });
            };
            entry.operation.id()
        };
        let operation = Operation::new(OperationPayload {
            parents: vec![parent],
            kind: OperationKind::Undo,
            relation: Some(atomic_core::operation::OperationRelation::Undo {
                target: target_operation,
            }),
            working_copy: None,
            before: RepoStateRef {
                view: None,
                working_copy: None,
                git: None,
            },
            delta: RepoStateDelta {
                after: RepoStateRef {
                    view: None,
                    working_copy: None,
                    git: None,
                },
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: vec![before_digest, after_digest],
            actor: ActorRef::System {
                name: "repository-path-claims-undo".to_string(),
            },
            timestamp_ms: super::operation::current_operation_timestamp_ms(),
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        txn.put_operation(&operation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(
            OperationScope::Repository,
            &[parent],
            &[operation.id()],
        )
        .map_err(pristine_error)?;
        let verified = super::operation::deterministic_effect_receipt(
            &operation,
            None,
            atomic_core::operation::EffectReceiptKind::Verified,
            None,
            None,
        )?;
        txn.append_effect_receipt(&verified)
            .map_err(pristine_error)?;
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(NativeIndexRepairOutcome {
            problems_repaired: 1,
            rows_written,
            already_healthy: false,
            operation: Some(operation.id()),
        })
    }

    /// Read the latest journaled path-claims repair's plan evidence and
    /// extract the complete old-row table for `undo_path_claims_repair`.
    /// The plan digest input's FIRST segment is the canonical old-table
    /// bytes; the journaled `missing_rows` reconstruction is unavailable
    /// from digests alone, so the caller supplies the old rows captured by
    /// the same transaction — stored here as the repair's reconstructible
    /// plan payload via `store_path_claims_repair_inverse`.
    pub fn undo_last_path_claims_repair(
        &self,
    ) -> Result<Option<NativeIndexRepairOutcome>, RepositoryError> {
        // The latest repository-scope Repair operation with a verified
        // receipt whose evidence carries the stored inverse payload.
        let log = self.operation_log(
            atomic_core::operation::OperationScope::Repository,
            None,
            false,
        )?;
        let OperationHeadState::Single(_head) = &log.head_state else {
            return Ok(None);
        };
        let Some(entry) = log.entries.iter().find(|entry| entry.is_head) else {
            return Ok(None);
        };
        let operation = &entry.operation;
        if operation.payload().kind != OperationKind::Repair {
            return Ok(None);
        }
        // The inverse payload is stored beside the operation id.
        let inverse_path = self.dot_dir.join("operation-recovery").join(format!(
            "path-claims-repair-inverse-{}.json",
            operation.id()
        ));
        let bytes = match std::fs::read(&inverse_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(RepositoryError::InvalidOperation {
                    message: "cannot read the stored path-claims repair inverse".to_string(),
                })
            }
        };
        let stored: BTreeMap<String, Vec<Vec<u8>>> =
            serde_json::from_slice(&bytes).map_err(|error| RepositoryError::InvalidOperation {
                message: format!("the stored path-claims repair inverse is malformed: {error}"),
            })?;
        let mut old_rows: BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>> = BTreeMap::new();
        for (path, rows) in stored {
            let mut fixed = Vec::new();
            for row in rows {
                let array: [u8; PATH_CLAIM_EVENT_SIZE] =
                    row.try_into()
                        .map_err(|_| RepositoryError::InvalidOperation {
                            message: "the stored inverse row is not the claim-event width"
                                .to_string(),
                        })?;
                fixed.push(array);
            }
            old_rows.insert(path, fixed);
        }
        self.undo_path_claims_repair(&old_rows).map(Some)
    }

    /// Store the reconstructible old-row table for a path-claims repair
    /// (CB-13A follow-up R3 "a stored repair inverse"). Called by the
    /// targeted repair right before journaling; the file is create-only and
    /// named by the journaled operation id.
    fn store_path_claims_repair_inverse(
        &self,
        operation: &atomic_core::operation::Operation,
        old_rows: &BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>>,
    ) -> Result<(), RepositoryError> {
        let recovery_dir = self.dot_dir.join("operation-recovery");
        std::fs::create_dir_all(&recovery_dir)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let path = recovery_dir.join(format!(
            "path-claims-repair-inverse-{}.json",
            operation.id()
        ));
        // Create-only: an existing file is never overwritten (idempotent
        // replays keep the original inverse).
        if path.exists() {
            return Ok(());
        }
        let stored: BTreeMap<String, Vec<Vec<u8>>> = old_rows
            .iter()
            .map(|(path, rows)| {
                (
                    path.clone(),
                    rows.iter().map(|row| row.to_vec()).collect::<Vec<_>>(),
                )
            })
            .collect();
        let bytes = serde_json::to_vec(&stored)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let temporary = recovery_dir.join(format!(
            ".path-claims-repair-inverse-{}.tmp",
            std::process::id()
        ));
        std::fs::write(&temporary, &bytes)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(())
    }

    fn repair_native_derived_indexes_inner(
        &self,
        fail_before_commit: bool,
    ) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        // CB-13A R3: same immutable journaling contract as the targeted
        // repair — one immediately durable transaction under the common
        // operation boundary contains the head resolution, the replacement,
        // its post-write verification, and the Repair operation with its
        // verified receipt. The injected failure aborts the whole
        // transaction: nothing is mutated and nothing is journaled.
        let common = self.try_lock_common_operation()?;
        let mut txn = common.begin_write_immediate()?;
        let parent = self.ensure_repository_anchor_in_txn(
            &mut txn,
            &RepoStateRef {
                view: None,
                working_copy: None,
                git: None,
            },
        )?;
        let expected = self.build_native_derived_indexes(&*txn)?;
        let before = compare_native_derived_indexes(&*txn, &expected)?;
        if before.is_healthy() {
            txn.abort()?;
            return Ok(NativeIndexRepairOutcome {
                already_healthy: true,
                rows_written: native_row_count(&expected),
                ..NativeIndexRepairOutcome::default()
            });
        }

        let before_digest = Hash::of(&canonical_tagged_bytes(
            b"repair:native-index:before",
            &canonical_index_problems_bytes(&before),
        ));
        (*txn)
            .replace_native_derived_indexes(&expected)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let after = compare_native_derived_indexes(&*txn, &expected)?;
        if !after.is_healthy() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "native-index replacement failed post-write verification with {} problem(s)",
                    after.problems.len()
                ),
            });
        }
        if fail_before_commit {
            return Err(RepositoryError::InvalidOperation {
                message: "injected native-index repair failure before commit".to_string(),
            });
        }
        let after_digest = Hash::of(&canonical_tagged_bytes(
            b"repair:native-index:after",
            &native_row_count(&expected).to_le_bytes(),
        ));
        let operation = Operation::new(OperationPayload {
            parents: vec![parent],
            kind: OperationKind::Repair,
            relation: None,
            working_copy: None,
            before: RepoStateRef {
                view: None,
                working_copy: None,
                git: None,
            },
            delta: RepoStateDelta {
                after: RepoStateRef {
                    view: None,
                    working_copy: None,
                    git: None,
                },
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: vec![before_digest, after_digest],
            actor: ActorRef::System {
                name: "repository-native-index-repair".to_string(),
            },
            timestamp_ms: super::operation::current_operation_timestamp_ms(),
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        txn.put_operation(&operation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(
            OperationScope::Repository,
            &[parent],
            &[operation.id()],
        )
        .map_err(pristine_error)?;
        let verified = super::operation::deterministic_effect_receipt(
            &operation,
            None,
            atomic_core::operation::EffectReceiptKind::Verified,
            None,
            None,
        )?;
        txn.append_effect_receipt(&verified)
            .map_err(pristine_error)?;
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(NativeIndexRepairOutcome {
            problems_repaired: before.problems.len(),
            rows_written: native_row_count(&expected),
            already_healthy: false,
            operation: Some(operation.id()),
        })
    }

    #[cfg(test)]
    pub(super) fn repair_native_derived_indexes_with_injected_failure(
        &self,
    ) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        self.repair_native_derived_indexes_inner(true)
    }

    fn build_native_derived_indexes<T>(
        &self,
        txn: &T,
    ) -> Result<NativeDerivedIndexes, RepositoryError>
    where
        T: GraphTxnT
            + TreeTxnT
            + PathClaimTxnT
            + ViewTxnT
            + InodeGraphOps<InodeError = PristineError>,
    {
        let (claims, recovered) = derive_native_path_claim_authority(self, txn)?;
        let mut expected = NativeDerivedIndexes {
            path_claims: claims.clone(),
            ..NativeDerivedIndexes::default()
        };
        let mut inodes: Vec<_> = recovered
            .iter()
            .map(|(position, inode)| (*inode, *position))
            .collect();
        inodes.sort_by_key(|(inode, position)| (inode.get(), *position));
        inodes.dedup();
        let mut inode_owners = BTreeMap::<Inode, Position<NodeId>>::new();
        for (inode, position) in &inodes {
            if let Some(previous) = inode_owners.insert(*inode, *position) {
                if previous != *position {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "inode {} owns both graph roots {} and {}",
                            inode.get(),
                            previous,
                            position
                        ),
                    });
                }
            }
        }
        expected.inodes = inodes;
        expected.rev_inodes = expected
            .inodes
            .iter()
            .map(|(inode, position)| (*position, *inode))
            .collect();
        expected.rev_inodes.sort();

        let current_view = txn
            .get_view(&self.current_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;
        let mut current_reduced = None;
        let mut conflicts = BTreeMap::<(u64, Inode), Vec<StoredConflict>>::new();

        let view_snapshot = txn
            .snapshot_views()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let view_names: Vec<String> = view_snapshot.iter().map(|(name, _)| name.clone()).collect();
        for view_name in &view_names {
            let view = txn
                .get_view(view_name)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.clone(),
                })?;
            let full_visibility = graph_visibility_closure(txn, &view)?;
            let claim_visibility =
                super::name_resolution::path_claim_visibility_for_view_with_entries(
                    txn,
                    &self.change_store,
                    &view,
                    &full_visibility,
                    &claims,
                )?;
            let reduced = super::name_resolution::reduce_path_claim_entries_with_inodes(
                txn,
                &claim_visibility,
                &claims,
                &recovered,
            )?;

            collect_expected_conflicts(
                txn,
                &self.change_store,
                view.id,
                &claim_visibility,
                &reduced,
                &mut conflicts,
            )?;
            if view.id == current_view.id {
                current_reduced = Some(reduced);
            }
        }

        let current_reduced = current_reduced.ok_or_else(|| RepositoryError::ViewNotFound {
            name: self.current_view.clone(),
        })?;
        let mut tree = BTreeMap::<String, Inode>::new();
        let mut reverse = BTreeMap::<Inode, String>::new();
        for side in &current_reduced.present {
            insert_tree_pair(&mut tree, &mut reverse, &side.path, side.inode)?;
        }

        let actual_tree = txn
            .iter_tree()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let actual_reverse: BTreeMap<_, _> = txn
            .iter_rev_tree_pairs()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .collect();
        let actual_inodes: BTreeSet<_> = txn
            .snapshot_inodes()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .map(|(inode, _)| inode)
            .collect();
        let actual_directories: BTreeMap<_, _> = txn
            .snapshot_directories()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .collect();
        let recorded_inodes: BTreeSet<_> =
            expected.inodes.iter().map(|(inode, _)| *inode).collect();
        let mut staged_directories = BTreeSet::new();
        let mut staged_paths = BTreeSet::new();
        for (path, inode) in actual_tree {
            if recorded_inodes.contains(&inode) {
                continue;
            }
            if actual_inodes.contains(&inode) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graphless staged path '{}' for inode {} has an unexpected INODES binding",
                        path,
                        inode.get()
                    ),
                });
            }
            if actual_reverse.get(&inode).map(String::as_str) != Some(path.as_str()) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graphless staged TREE row '{}' for inode {} is not bijective",
                        path,
                        inode.get()
                    ),
                });
            }
            insert_tree_pair(&mut tree, &mut reverse, &path, inode)?;
            staged_paths.insert(path.clone());
            if let Some(flags) = actual_directories.get(&inode) {
                let allowed = directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY;
                if flags & !allowed != 0 {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "graphless staged directory inode {} has invalid flags {flags:#04x}",
                            inode.get()
                        ),
                    });
                }
                staged_directories.insert(inode);
            }
        }
        for (inode, path) in &actual_reverse {
            if recorded_inodes.contains(inode) {
                continue;
            }
            if actual_inodes.contains(inode) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graphless staged reverse path '{}' for inode {} has an unexpected INODES binding",
                        path,
                        inode.get()
                    ),
                });
            }
            if tree.get(path) != Some(inode) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graphless staged REV_TREE row for inode {} and '{}' is not bijective",
                        inode.get(),
                        path
                    ),
                });
            }
        }

        expected.tree = tree
            .iter()
            .map(|(path, inode)| (path.clone(), *inode))
            .collect();
        expected.rev_tree = reverse
            .iter()
            .map(|(inode, path)| (*inode, path.clone()))
            .collect();

        let mut alive_paths = BTreeSet::new();
        let mut directory_paths = BTreeMap::<Inode, BTreeSet<String>>::new();
        for side in &current_reduced.present {
            alive_paths.insert(side.path.clone());
            if side.kind == PathClaimKind::Directory {
                directory_paths
                    .entry(side.inode)
                    .or_default()
                    .insert(side.path.clone());
            }
        }
        for conflict in current_reduced.conflicts.values() {
            for side in &conflict.sides {
                alive_paths.insert(side.path.clone());
                if side.kind == PathClaimKind::Directory {
                    directory_paths
                        .entry(side.inode)
                        .or_default()
                        .insert(side.path.clone());
                }
            }
        }
        alive_paths.extend(staged_paths);
        for inode in staged_directories {
            if let Some(path) = reverse.get(&inode) {
                alive_paths.insert(path.clone());
                directory_paths
                    .entry(inode)
                    .or_default()
                    .insert(path.clone());
            }
        }
        expected.directories = directory_paths
            .into_iter()
            .map(|(inode, paths)| {
                let has_child = paths.iter().any(|directory| {
                    alive_paths
                        .iter()
                        .any(|candidate| parent_path(candidate) == Some(directory.as_str()))
                });
                let flags = directory_flags::DIR_EXPLICIT
                    | if has_child {
                        0
                    } else {
                        directory_flags::DIR_EMPTY
                    };
                (inode, flags)
            })
            .collect();
        expected.directories.sort_by_key(|(inode, _)| inode.get());

        expected.conflicts = conflicts
            .into_iter()
            .map(|((view_id, inode), mut records)| {
                records.sort_by(|left, right| {
                    (&left.path, left.kind.to_string(), left.line, &left.sides).cmp(&(
                        &right.path,
                        right.kind.to_string(),
                        right.line,
                        &right.sides,
                    ))
                });
                records.dedup();
                (view_id, inode, records)
            })
            .collect();
        Ok(expected)
    }
}

fn collect_expected_conflicts<T>(
    txn: &T,
    store: &ChangeStore,
    view_id: u64,
    visibility: &GraphVisibilityClosure,
    reduced: &super::name_resolution::ReducedPathClaims,
    conflicts: &mut BTreeMap<(u64, Inode), Vec<StoredConflict>>,
) -> Result<(), RepositoryError>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps<InodeError = PristineError>,
{
    for (path, conflict) in &reduced.conflicts {
        let mut side_hashes = Vec::new();
        for change_id in conflict
            .sides
            .iter()
            .flat_map(|side| side.event_changes.iter().copied())
        {
            let hash = txn
                .get_external(change_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "name-conflict side change {} has no external hash",
                        change_id.get()
                    ))
                })?;
            side_hashes.push(hash.to_base32());
        }
        side_hashes.sort();
        side_hashes.dedup();
        for side in conflict.sides_at_path(path) {
            conflicts
                .entry((view_id, side.inode))
                .or_default()
                .push(StoredConflict {
                    kind: StoredConflictKind::Name,
                    path: path.clone(),
                    line: (!side.is_directory()).then_some(1),
                    sides: side_hashes.clone(),
                });
        }
    }

    for side in &reduced.present {
        if side.is_directory() {
            continue;
        }
        // A persisted order conflict comes from the *renderer* actually
        // emitting conflict markers, never from marker-shaped source content
        // (documentation examples, harness fixtures). `conflicts` is non-empty
        // only when the output layer rendered a real conflict region for this
        // file, so only that can produce a conflict row.
        let (content, rendered_conflicts, _had_fork_structure) =
            atomic_core::output::repo::output_file_to_buffer_with_options(
                txn,
                store,
                side.position,
                atomic_core::output::repo::FileOutputOptions::new(),
                atomic_core::output::alive::RetrieveOptions::new()
                    .with_graph_visibility(visibility.clone()),
            )
            .map_err(|error| RepositoryError::Output(error.to_string()))?;
        if rendered_conflicts.is_empty() {
            continue;
        }
        if let Some(line) = super::materialize::first_conflict_marker_line(&content) {
            conflicts
                .entry((view_id, side.inode))
                .or_default()
                .push(StoredConflict {
                    kind: StoredConflictKind::Order,
                    path: side.path.clone(),
                    line: Some(line),
                    sides: Vec::new(),
                });
        }
    }
    Ok(())
}

fn visit_reachable_change<T: GraphTxnT>(
    txn: &T,
    change_id: NodeId,
    reachable: &HashSet<NodeId>,
    visiting: &mut HashSet<NodeId>,
    visited: &mut HashSet<NodeId>,
    ordered: &mut Vec<NodeId>,
) -> Result<(), RepositoryError> {
    if visited.contains(&change_id) {
        return Ok(());
    }
    if !visiting.insert(change_id) {
        return Err(RepositoryError::InvalidOperation {
            message: format!("native-index replay found a dependency cycle at {change_id}"),
        });
    }
    for dependency in txn
        .get_indexed_change_deps(change_id)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let dependency_id = txn
            .get_internal(&dependency)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "change {} has unregistered dependency {}",
                    change_id.get(),
                    dependency
                ))
            })?;
        if reachable.contains(&dependency_id) {
            visit_reachable_change(txn, dependency_id, reachable, visiting, visited, ordered)?;
        }
    }
    visiting.remove(&change_id);
    visited.insert(change_id);
    ordered.push(change_id);
    Ok(())
}

fn insert_tree_pair(
    tree: &mut BTreeMap<String, Inode>,
    reverse: &mut BTreeMap<Inode, String>,
    path: &str,
    inode: Inode,
) -> Result<(), RepositoryError> {
    if let Some(previous) = tree.insert(path.to_string(), inode) {
        if previous != inode {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "native projection maps '{}' to both inode {} and inode {}",
                    path,
                    previous.get(),
                    inode.get()
                ),
            });
        }
    }
    if let Some(previous) = reverse.insert(inode, path.to_string()) {
        if previous != path {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "native projection maps inode {} to both '{}' and '{}'",
                    inode.get(),
                    previous,
                    path
                ),
            });
        }
    }
    Ok(())
}

fn parent_path(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn compare_native_derived_indexes<T>(
    txn: &T,
    expected: &NativeDerivedIndexes,
) -> Result<NativeIndexReport, RepositoryError>
where
    T: TreeTxnT + PathClaimTxnT + ViewTxnT,
{
    let mut report = NativeIndexReport {
        expected_rows: native_row_count(expected),
        ..NativeIndexReport::default()
    };

    match txn.path_claim_schema_version() {
        Ok(Some(version)) if version == atomic_core::pristine::PATH_CLAIM_SCHEMA_VERSION => {}
        Ok(version) => report.problems.push(NativeIndexProblem {
            index: NativeIndex::PathClaims,
            kind: if version.is_none() {
                NativeIndexProblemKind::Missing
            } else {
                NativeIndexProblemKind::Mismatched
            },
            key: "<schema>".to_string(),
            expected: Some(atomic_core::pristine::PATH_CLAIM_SCHEMA_VERSION.to_string()),
            actual: version.map(|version| version.to_string()),
        }),
        Err(error) => report.problems.push(NativeIndexProblem {
            index: NativeIndex::PathClaims,
            kind: NativeIndexProblemKind::Malformed,
            key: "<schema>".to_string(),
            expected: Some(atomic_core::pristine::PATH_CLAIM_SCHEMA_VERSION.to_string()),
            actual: Some(error.to_string()),
        }),
    }
    compare_table(
        NativeIndex::PathClaims,
        canonical_path_claims(&expected.path_claims),
        txn.iter_path_claims()
            .map(|rows| canonical_path_claims(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::Tree,
        canonical_tree(&expected.tree),
        txn.iter_tree()
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map(|rows| canonical_tree(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::RevTree,
        canonical_rev_tree(&expected.rev_tree),
        txn.iter_rev_tree_pairs()
            .map(|rows| canonical_rev_tree(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::Inodes,
        canonical_inodes(&expected.inodes),
        txn.snapshot_inodes().map(|rows| canonical_inodes(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::RevInodes,
        canonical_rev_inodes(&expected.rev_inodes),
        txn.snapshot_rev_inodes()
            .map(|rows| canonical_rev_inodes(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::Directories,
        canonical_directories(&expected.directories),
        txn.snapshot_directories()
            .map(|rows| canonical_directories(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::Conflicts,
        canonical_conflicts(&expected.conflicts),
        txn.snapshot_conflicts()
            .map(|rows| canonical_conflicts(&rows)),
        &mut report,
    );
    report.problems.sort();
    Ok(report)
}

fn compare_table(
    index: NativeIndex,
    expected: BTreeMap<String, String>,
    actual: Result<BTreeMap<String, String>, PristineError>,
    report: &mut NativeIndexReport,
) {
    let actual = match actual {
        Ok(actual) => actual,
        Err(error) => {
            report.problems.push(NativeIndexProblem {
                index,
                kind: NativeIndexProblemKind::Malformed,
                key: "<table>".to_string(),
                expected: None,
                actual: Some(error.to_string()),
            });
            return;
        }
    };
    report.actual_rows += actual.len();
    for (key, expected_value) in &expected {
        match actual.get(key) {
            None => report.problems.push(NativeIndexProblem {
                index,
                kind: NativeIndexProblemKind::Missing,
                key: key.clone(),
                expected: Some(expected_value.clone()),
                actual: None,
            }),
            Some(actual_value) if actual_value != expected_value => {
                report.problems.push(NativeIndexProblem {
                    index,
                    kind: NativeIndexProblemKind::Mismatched,
                    key: key.clone(),
                    expected: Some(expected_value.clone()),
                    actual: Some(actual_value.clone()),
                });
            }
            Some(_) => {}
        }
    }
    for (key, actual_value) in actual {
        if !expected.contains_key(&key) {
            report.problems.push(NativeIndexProblem {
                index,
                kind: NativeIndexProblemKind::Stale,
                key,
                expected: None,
                actual: Some(actual_value),
            });
        }
    }
}

fn canonical_path_claims(rows: &[PathClaimEntry]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|entry| {
            let event = entry.event;
            (
                format!(
                    "{:?}",
                    (
                        &entry.path,
                        event.event_change,
                        event.operation_index,
                        event.kind,
                        event.state,
                        event.claim,
                    )
                ),
                "present".to_string(),
            )
        })
        .collect()
}

fn canonical_tree(rows: &[(String, Inode)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(path, inode)| (path.clone(), inode.get().to_string()))
        .collect()
}

fn canonical_rev_tree(rows: &[(Inode, String)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(inode, path)| (inode.get().to_string(), path.clone()))
        .collect()
}

fn canonical_inodes(rows: &[(Inode, Position<NodeId>)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(inode, position)| (inode.get().to_string(), position.to_string()))
        .collect()
}

fn canonical_rev_inodes(rows: &[(Position<NodeId>, Inode)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(position, inode)| (position.to_string(), inode.get().to_string()))
        .collect()
}

fn canonical_directories(rows: &[(Inode, u8)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(inode, flags)| (inode.get().to_string(), format!("{flags:#04x}")))
        .collect()
}

fn canonical_conflicts(rows: &[(u64, Inode, Vec<StoredConflict>)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(view_id, inode, records)| {
            let mut records = records.clone();
            records.sort_by(|left, right| {
                (&left.path, left.kind.to_string(), left.line, &left.sides).cmp(&(
                    &right.path,
                    right.kind.to_string(),
                    right.line,
                    &right.sides,
                ))
            });
            (format!("{view_id}:{}", inode.get()), format!("{records:?}"))
        })
        .collect()
}

fn native_row_count(indexes: &NativeDerivedIndexes) -> usize {
    indexes.path_claims.len()
        + indexes.tree.len()
        + indexes.rev_tree.len()
        + indexes.inodes.len()
        + indexes.rev_inodes.len()
        + indexes.directories.len()
        + indexes.conflicts.len()
}

/// Domain-separated canonical bytes for a Repair evidence digest: the tag
/// makes the before-plan and post-write digests structurally distinct even
/// when the canonical row encodings coincide.
fn canonical_tree_bijection_rows_bytes(
    stale: &[(u64, String)],
    missing: &[(String, u64)],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (inode, path) in stale {
        body.extend_from_slice(&inode.to_le_bytes());
        body.extend_from_slice(&(path.len() as u64).to_le_bytes());
        body.extend_from_slice(path.as_bytes());
    }
    for (path, inode) in missing {
        body.extend_from_slice(&inode.to_le_bytes());
        body.extend_from_slice(&(path.len() as u64).to_le_bytes());
        body.extend_from_slice(path.as_bytes());
    }
    body
}

fn canonical_reverse_rows_bytes(rows: &[(u64, String)]) -> Vec<u8> {
    let mut sorted: Vec<&(u64, String)> = rows.iter().collect();
    sorted.sort();
    let mut body = Vec::new();
    for (inode, path) in sorted {
        body.extend_from_slice(&inode.to_le_bytes());
        body.extend_from_slice(&(path.len() as u64).to_le_bytes());
        body.extend_from_slice(path.as_bytes());
    }
    body
}

fn canonical_tagged_bytes(tag: &[u8], body: &[u8]) -> Vec<u8> {
    let mut bytes = tag.to_vec();
    bytes.push(0);
    bytes.extend_from_slice(body);
    bytes
}

/// Canonical bytes of a targeted repair plan (sorted path/event pairs), used
/// as the immutable before-evidence digest of the journaled Repair operation.
fn canonical_missing_rows_bytes(rows: &[(String, [u8; PATH_CLAIM_EVENT_SIZE])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (path, event) in rows {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(event);
    }
    bytes
}

/// Canonical bytes of a post-write PATH_CLAIMS table state, used as the
/// immutable after-evidence digest of the journaled Repair operation.
fn canonical_path_claim_rows_bytes(
    rows: &BTreeMap<String, Vec<[u8; PATH_CLAIM_EVENT_SIZE]>>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (path, events) in rows {
        let mut sorted = events.clone();
        sorted.sort();
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
        for event in sorted {
            bytes.extend_from_slice(&event);
        }
    }
    bytes
}

/// Canonical bytes of a verification report, used as the immutable
/// before-evidence digest of a full native-index rebuild.
fn canonical_index_problems_bytes(report: &NativeIndexReport) -> Vec<u8> {
    let mut bytes = Vec::new();
    for problem in &report.problems {
        bytes.extend_from_slice(format!("{problem}\n").as_bytes());
    }
    bytes
}
