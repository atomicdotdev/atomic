use super::*;
use crate::apply::InsertOptions;
use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind, RepoStateRef,
    ViewStateRef,
};
use atomic_core::pristine::{
    CachedGraphTxn, GraphTxnT, GraphVisibilityClosure, PathClaimId, ViewGraph, ViewMembershipSet,
};

/// Canonical trunk, live lines in closure order, and all observed line identities.
type ExistingSemanticState = (
    Option<atomic_core::crdt::TrunkId>,
    Vec<atomic_core::crdt::BranchId>,
    Vec<atomic_core::crdt::BranchId>,
);

/// Walk the line order recorded by one effective closure. Ambient AFTER
/// rows may have been reparented by a different view and are not a baseline.
pub(super) fn semantic_branch_order(
    after: &std::collections::BTreeMap<
        atomic_core::crdt::BranchId,
        Option<atomic_core::crdt::BranchId>,
    >,
) -> Result<Vec<atomic_core::crdt::BranchId>, String> {
    let mut successors = std::collections::BTreeMap::<_, Vec<_>>::new();
    for (branch, predecessor) in after {
        if predecessor.is_some_and(|id| !after.contains_key(&id)) {
            return Err(format!(
                "semantic branch {branch:?} has a predecessor outside its closure"
            ));
        }
        successors.entry(*predecessor).or_default().push(*branch);
    }
    let mut stack = successors.remove(&None).unwrap_or_default();
    stack.reverse();
    let mut ordered = Vec::with_capacity(after.len());
    while let Some(branch) = stack.pop() {
        ordered.push(branch);
        if let Some(mut children) = successors.remove(&Some(branch)) {
            children.reverse();
            stack.extend(children);
        }
    }
    if ordered.len() != after.len() {
        return Err("semantic closure contains a cyclic line order".into());
    }
    Ok(ordered)
}

/// A whole-file resolution supersedes every line identity observed in its
/// parent closure. A line that one replay considers dead can still be alive
/// in the ambient table after a concurrent Modify. Persist its deletion too,
/// so the resolution dominates both histories in either ingestion order.
fn complete_semantic_replacement(
    mut recorded: atomic_core::record::workflow::RecordedFile,
    observed: &[atomic_core::crdt::BranchId],
) -> atomic_core::record::workflow::RecordedFile {
    if recorded
        .hunks()
        .iter()
        .any(|hunk| hunk.deleted_lines == [usize::MAX])
    {
        if let Some(mut ops) = recorded.crdt_ops().cloned() {
            let addressed: std::collections::HashSet<_> =
                ops.line_ops().iter().map(|line| line.branch_id()).collect();
            for branch in observed {
                if !addressed.contains(branch) {
                    ops.add_line_op(atomic_core::change::LineOps::delete_empty(*branch));
                }
            }
            recorded.set_crdt_ops(ops);
        }
    }
    recorded
}

#[cfg(test)]
thread_local! {
    pub(super) static RECORD_APPLY_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, Clone)]
struct FileMovePlan {
    old_path: String,
    new_path: String,
    inode: Inode,
    claimant: Position<NodeId>,
    source: PathClaimId,
    old_content: Vec<u8>,
}

#[derive(Debug, Default)]
struct FileMovePlanning {
    moves: Vec<FileMovePlan>,
    evidence: crate::record::MoveEvidence,
    unresolved_destinations: std::collections::BTreeSet<String>,
    fresh_additions: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct PreparedNameResolution {
    winner: super::name_resolution::ProjectedPathClaim,
    operation: GraphOp<Option<Hash>>,
    dependencies: Vec<Hash>,
    conflict_inodes: Vec<Inode>,
}

/// Build the record outcome that reports a scoped stale-conflict cleanup which
/// cleared rows without recording any content change.
fn conflict_cleanup_outcome(
    header: ChangeHeader,
    cleanup: ConflictReconcileOutcome,
) -> RecordOutcome {
    let change = Change::empty(header);
    let mut outcome = RecordOutcome::new(change, Hash::ZERO, RecordStats::new());
    outcome.set_conflict_cleanup(crate::record::ConflictCleanupSummary {
        view: cleanup.view,
        paths: cleanup.cleared_paths,
        rows_cleared: cleanup.cleared_rows,
        operation: cleanup.operation,
    });
    outcome
}

impl Repository {
    /// CB-9B: record one merge-resolution or rewritten-history modification
    /// as a graph-safe replace with regenerated semantic FileOps and stable
    /// CRDT identities (review blocker 3).
    ///
    /// The baseline is the bytes the view's graph renders for `path` — never
    /// a Git worktree read. When the baseline required fork/cyclic conflict
    /// resolution (a merge union with both spans alive) or
    /// `conflict_baseline` is set, the whole-file-replace safety path is
    /// forced: "what's alive gets deleted, this is inserted fresh", which is
    /// exactly a merge resolution. Delete/Modify CRDT ops bind to the real
    /// existing alive branches, so the semantic layer stays coherent after
    /// the resolution. Errors fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error message when the path has no inode binding, content
    /// retrieval fails, or the recorded result is empty.
    pub fn record_resolution_modified_file(
        &self,
        path: &str,
        new_content: &[u8],
        view_name: &str,
        conflict_baseline: bool,
    ) -> Result<atomic_core::record::workflow::RecordedFile, String> {
        use atomic_core::output::alive::RetrieveOptions;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| format!("cannot open pristine: {e}"))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| format!("cannot read view: {e}"))?
            .ok_or_else(|| format!("view '{view_name}' does not exist"))?;
        let visibility = super::filter::graph_visibility_closure(&txn, &view)
            .map_err(|e| format!("cannot build view visibility: {e}"))?;

        let inode = txn
            .get_inode(path)
            .map_err(|e| format!("cannot read inode for '{path}': {e}"))?
            .ok_or_else(|| format!("path '{path}' has no inode binding"))?;
        let position = txn
            .inode_position(inode)
            .map_err(|e| format!("cannot read position for '{path}': {e}"))?
            .ok_or_else(|| format!("inode {inode:?} has no graph position"))?;

        let (old_content, had_fork_structure) =
            super::content::retrieve_content_with_filter_fast_with_fork_info(
                &txn,
                &self.change_store,
                inode,
                position,
                RetrieveOptions::new().with_graph_visibility(visibility.clone()),
            )
            .map_err(|e| format!("cannot retrieve baseline for '{path}': {e}"))?;

        self.record_foreign_modified_file(
            path,
            &old_content,
            new_content,
            conflict_baseline || had_fork_structure,
        )
    }

    /// Record an explicit foreign merge result for a same-path file conflict.
    /// Uses the native name-resolution policy and canonical claims; no TREE
    /// binding or import order chooses the retained inode. Returns None when
    /// the selected closure has no name conflict at this path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(repo: &atomic_repository::Repository) -> Result<(), String> {
    /// let records = repo.record_foreign_name_resolution_on_view_excluding(
    ///     "config.txt", b"merged settings\n", "main", &[],
    /// )?;
    /// // Include these records in the GitResolution for the parent union.
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    /// Refuses directory or rename conflicts and unreadable graph or semantic
    /// state. Equal-content claimants in an explicit foreign merge are resolved
    /// by canonical identity, with the losing claims retained in history.
    pub fn record_foreign_name_resolution_on_view_excluding(
        &self,
        path: &str,
        new_content: &[u8],
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<Option<Vec<atomic_core::record::workflow::RecordedFile>>, String> {
        use atomic_core::output::alive::RetrieveOptions;
        use atomic_core::record::workflow::{
            record_modified_file, DetectedFile, RecordedFile, RecordingOptions,
        };
        let txn = self.pristine.read_txn().map_err(|e| e.to_string())?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("view '{view_name}' does not exist"))?;
        let visibility = super::filter::assembly_visibility_excluding(&txn, &view, excluded)
            .map_err(|e| e.to_string())?;
        let projection = self
            .project_tree_for_visibility_scoped(&txn, &visibility, Some(path))
            .map_err(|e| e.to_string())?;
        let Some(conflict) = projection.name_conflicts.get(path) else {
            return Ok(None);
        };
        let cached = CachedGraphTxn::new(&txn).map_err(|e| e.to_string())?;
        let resolution = prepare_name_resolution(
            &txn,
            &cached,
            &self.change_store,
            &visibility,
            path,
            conflict,
            new_content,
            NameResolutionPolicy::ForeignMerge,
        )
        .map_err(|e| e.to_string())?;
        let winner = resolution.winner;
        let mut records = Vec::new();
        for side in conflict.sides_at_path(path) {
            if side.inode == winner.inode {
                continue;
            }
            let bindings = super::materialize::live_names_for_path(
                &txn,
                &self.change_store,
                side.position,
                path,
                &visibility.iter_dependency_first().copied().collect(),
            )
            .map_err(|e| e.to_string())?;
            let mut unlinked = RecordedFile::new(path);
            unlinked.set_inode(side.inode);
            unlinked.set_position(side.position);
            unlinked.set_name_conflict_resolution(winner.position, bindings);
            // Match native resolution: unlink losing names, retain their
            // graph content and semantic identities as historical evidence.
            let (trunk, _, _) =
                self.resolve_existing_semantic_state(path, side.inode, view_name, excluded)?;
            if let Some(trunk) = trunk {
                unlinked.set_crdt_ops(atomic_core::change::FileOps::new(
                    trunk,
                    path.to_string(),
                    None,
                ));
            }
            records.push(unlinked);
        }
        let (old_content, _) = super::content::retrieve_content_with_filter_fast_with_fork_info(
            &txn,
            &self.change_store,
            winner.inode,
            winner.position,
            RetrieveOptions::new().with_graph_visibility(visibility),
        )
        .map_err(|e| e.to_string())?;
        if old_content != new_content {
            let (trunk, branches, observed) =
                self.resolve_existing_semantic_state(path, winner.inode, view_name, excluded)?;
            let memory = atomic_core::output::memory::Memory::new();
            memory.add_file(path, new_content);
            let mut detected = DetectedFile::modified(path);
            detected.inode = Some(winner.inode);
            detected.position = Some(winner.position);
            let record = record_modified_file(
                &memory,
                &detected,
                &old_content,
                None,
                &RecordingOptions::new()
                    .unlimited_file_size()
                    .force_whole_file_replace(true),
                trunk,
                Some(&branches),
            )?;
            records.push(complete_semantic_replacement(record, &observed));
        } else {
            // The foreign merge may resolve permissions/kind even when its
            // bytes equal the retained claimant. Expose that exact survivor
            // for the caller's attribute writes, never a losing path claimant.
            let mut survivor = RecordedFile::new(path);
            survivor.set_kind(atomic_core::record::workflow::DetectionKind::Modified);
            survivor.set_inode(winner.inode);
            survivor.set_position(winner.position);
            records.push(survivor);
        }
        Ok(Some(records))
    }

    /// Paths with causally concurrent semantic writers in the selected closure.
    ///
    /// A foreign merge must explicitly resolve these histories even when their
    /// graph bytes agree. FileOps and dependency edges establish concurrency;
    /// canonical FileAdd positions and filtered path claims establish identity.
    /// No Git merge-base heuristic or ambient semantic row selects a writer.
    /// Errors reading canonical changes, dependencies, or path claims propagate.
    pub fn foreign_merge_concurrent_semantic_paths(
        &self,
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<std::collections::BTreeSet<String>, String> {
        use atomic_core::crdt::{BranchId, TrunkId, TrunkOp};
        use std::collections::{HashMap, HashSet};
        let txn = self.pristine.read_txn().map_err(|e| e.to_string())?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("view '{view_name}' does not exist"))?;
        let visibility = super::filter::assembly_visibility_excluding(&txn, &view, excluded)
            .map_err(|e| e.to_string())?;
        let mut dependencies: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut writers: HashMap<TrunkId, HashSet<NodeId>> = HashMap::new();
        let mut line_writers: HashMap<(TrunkId, BranchId), HashSet<NodeId>> = HashMap::new();
        let mut positions = HashMap::new();
        for id in visibility
            .iter_dependency_first()
            .copied()
            .filter(|id| !id.is_root())
        {
            let hash = txn
                .get_external(id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("closure change {id} has no hash"))?;
            let change = self.load_change(&hash).map_err(|e| e.to_string())?;
            let mut parents = Vec::new();
            for hash in change.dependencies() {
                let parent = txn
                    .get_internal(hash)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("change {id} has an unregistered dependency {hash}"))?;
                if !parent.is_root() && !dependencies.contains_key(&parent) {
                    return Err(format!(
                        "dependency {parent} is missing before closure change {id}"
                    ));
                }
                parents.push(parent);
            }
            let mut ancestors = HashSet::new();
            let mut pending = parents.clone();
            while let Some(parent) = pending.pop() {
                if ancestors.insert(parent) {
                    if let Some(prior) = dependencies.get(&parent) {
                        pending.extend(prior.iter().copied());
                    }
                }
            }
            dependencies.insert(id, parents);
            for ops in change.file_ops() {
                let raw = ops.trunk_id();
                let trunk = if raw.change_id().is_root() {
                    TrunkId::new(id, raw.file_idx())
                } else {
                    raw
                };
                if matches!(ops.trunk_op(), Some(TrunkOp::Create { .. })) {
                    for hunk in change.hunks() {
                        if let GraphOp::FileAdd {
                            path, add_inode, ..
                        } = hunk
                        {
                            if path == ops.path() {
                                positions.insert(Position::new(id, add_inode.start), trunk);
                            }
                        }
                    }
                }
                if ops.trunk_op().is_some() || !ops.line_ops().is_empty() {
                    let frontier = writers.entry(trunk).or_default();
                    frontier.retain(|writer| !ancestors.contains(writer));
                    frontier.insert(id);
                }
                // A later edit to another line does not resolve this line's
                // concurrent writers merely by depending on both histories.
                for line in ops.line_ops() {
                    let raw = line.branch_id();
                    let branch = if raw.change_id().is_root() {
                        BranchId::new(id, raw.branch_idx())
                    } else {
                        raw
                    };
                    let frontier = line_writers.entry((trunk, branch)).or_default();
                    frontier.retain(|writer| !ancestors.contains(writer));
                    frontier.insert(id);
                }
            }
        }
        let mut concurrent: HashSet<_> = writers
            .iter()
            .filter(|(_, frontier)| frontier.len() > 1)
            .map(|(trunk, _)| *trunk)
            .collect();
        concurrent.extend(
            line_writers
                .iter()
                .filter(|(_, frontier)| frontier.len() > 1)
                .map(|((trunk, _), _)| *trunk),
        );
        let claims = super::name_resolution::reduce_path_claims(&txn, &visibility)
            .map_err(|e| e.to_string())?;
        Ok(claims
            .present
            .iter()
            .chain(
                claims
                    .conflicts
                    .values()
                    .flat_map(|conflict| conflict.sides.iter()),
            )
            .filter(|claim| {
                positions
                    .get(&claim.position)
                    .is_some_and(|trunk| concurrent.contains(trunk))
            })
            .map(|claim| claim.path.clone())
            .collect())
    }

    /// Resolve directory identities selected by a foreign merge tree, retaining
    /// surviving children through explicit same-path structural moves.
    ///
    /// Git directories have no inode identity. Independently introduced
    /// directories therefore need a canonical name resolution in addition to
    /// any file resolutions. The returned records must precede `file_records`
    /// so assembly uses the selected directory anchors for new children too.
    /// All claims come from the exact parent closure; no TREE binding selects
    /// the winner and no losing directory's children are silently discarded.
    pub fn record_foreign_directory_resolutions_on_view_excluding(
        &self,
        directories: &std::collections::BTreeSet<String>,
        file_records: &[atomic_core::record::workflow::RecordedFile],
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<Vec<atomic_core::record::workflow::RecordedFile>, String> {
        use atomic_core::record::workflow::{DetectionKind, RecordedFile};
        let txn = self.pristine.read_txn().map_err(|e| e.to_string())?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("view '{view_name}' does not exist"))?;
        let visibility = super::filter::assembly_visibility_excluding(&txn, &view, excluded)
            .map_err(|e| e.to_string())?;
        let reduced = super::name_resolution::reduce_path_claims(&txn, &visibility)
            .map_err(|e| e.to_string())?;
        let mut conflicts: Vec<_> = reduced
            .conflicts
            .iter()
            .filter(|(path, conflict)| {
                directories.contains(*path) && conflict.sides.iter().any(|side| side.is_directory())
            })
            .collect();
        conflicts.sort_by_key(|(path, _)| (path.matches('/').count(), *path));
        let mut records = Vec::new();
        let mut replaced_parents = std::collections::HashSet::new();
        let mut losing = std::collections::HashSet::new();
        for record in file_records {
            if record.name_conflict_resolution().is_some()
                || record.kind() == Some(DetectionKind::Deleted)
            {
                if let Some(position) = record.position() {
                    losing.insert(position);
                }
            }
        }
        for (path, conflict) in conflicts {
            if conflict.is_rename_conflict()
                || conflict.sides.iter().any(|side| !side.is_directory())
            {
                return Err(format!(
                    "cannot resolve mixed-kind or renamed directory '{path}'"
                ));
            }
            let mut sides = Vec::new();
            for side in conflict.sides_at_path(path) {
                let hash = txn
                    .get_external(side.position.change)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("directory '{path}' has no canonical identity"))?;
                sides.push(((hash, side.position.pos), side));
            }
            sides.sort_by_key(|(key, _)| *key);
            let Some((_, winner)) = sides.first() else {
                continue;
            };
            for (_, side) in sides.iter().skip(1) {
                losing.insert(side.position);
                replaced_parents.insert(side.position);
                let mut record = RecordedFile::new(path);
                record.set_kind(DetectionKind::Modified);
                record.set_inode(side.inode);
                record.set_position(side.position);
                record.set_name_conflict_resolution(
                    winner.position,
                    side.claims.iter().map(|claim| claim.name).collect(),
                );
                records.push(record);
            }
        }
        // Resolve the directory names first, then reconnect each surviving
        // immediate child to the selected parent. Nested directories use the
        // same operation, retaining both their inode and their descendants.
        let mut children: Vec<_> = reduced
            .present
            .iter()
            .chain(
                reduced
                    .conflicts
                    .values()
                    .flat_map(|conflict| conflict.sides.iter()),
            )
            .collect();
        children.sort_by_key(|side| (&side.path, side.position));
        children.dedup_by_key(|side| (&side.path, side.position));
        for child in children {
            if losing.contains(&child.position) {
                continue;
            }
            for source in &child.claims {
                if !replaced_parents.contains(&source.parent.end_pos()) {
                    continue;
                }
                let mut record = RecordedFile::new(&child.path);
                record.set_kind(DetectionKind::Moved);
                record.set_old_path(child.path.clone());
                record.set_inode(child.inode);
                record.set_position(child.position);
                record.set_exact_source_claim(*source);
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Record a modified file against an explicit baseline while binding the
    /// edit to the file's existing CRDT trunk and alive branches (CB-9B
    /// review F4).
    ///
    /// The bridge importer drives this for foreign deltas: the baseline is
    /// the commit's own context (Git parent-tree bytes in leg mode), never
    /// silently re-read from the view. The semantic state resolution is the
    /// same as [`Self::record_resolution_modified_file`]: the file's current
    /// CRDT trunk is resolved by path (falling back to the inode index) and
    /// its alive branches are bound so delete/modify ops tombstone the real
    /// lines with stable identities. Errors fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error message when the path has no inode binding, content
    /// retrieval fails, or the recorded result is empty.
    pub fn record_foreign_modified_file(
        &self,
        path: &str,
        old_content: &[u8],
        new_content: &[u8],
        force_whole_file_replace: bool,
    ) -> Result<atomic_core::record::workflow::RecordedFile, String> {
        self.record_foreign_modified_file_on_view_excluding(
            path,
            old_content,
            new_content,
            force_whole_file_replace,
            &self.current_view,
            &[],
        )
    }

    /// Record a foreign edit using the same canonical closure as its baseline
    /// and staged verification. Derived TREE bindings may belong to another
    /// view, or be absent after an interrupted import.
    pub fn record_foreign_modified_file_on_view_excluding(
        &self,
        path: &str,
        old_content: &[u8],
        new_content: &[u8],
        force_whole_file_replace: bool,
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<atomic_core::record::workflow::RecordedFile, String> {
        let (inode, position) = self
            .get_inode_and_position_on_view_excluding(path, view_name, excluded)
            .map_err(|e| format!("cannot resolve '{path}' in view '{view_name}': {e}"))?
            .ok_or_else(|| format!("path '{path}' is absent from view '{view_name}'"))?;
        let (existing_trunk_id, existing_branches, observed) =
            self.resolve_existing_semantic_state(path, inode, view_name, excluded)?;
        use atomic_core::record::workflow::{record_modified_file, DetectedFile};
        let memory_wc = atomic_core::output::memory::Memory::new();
        memory_wc.add_file(path, new_content);
        let mut detected = DetectedFile::modified(path);
        detected.inode = Some(inode);
        detected.position = Some(position);
        // ::26 R1 fifth follow-up (2026-10-14): the alive semantic branch
        // count must equal the old content's line count — the positional
        // hunks below index branches by diff line number. Histories with
        // many accumulated in-place edits leave extra alive vertices and
        // branches (diamond remnants), so the counts drift and positional
        // anchoring corrupts the region. Force the sentinel whole-file
        // replace: tombstone every prior branch and rebuild fresh, so the
        // graph and semantic layers agree with the commit tree byte-exactly.
        let branch_line_divergence =
            existing_branches.len() != atomic_core::diff::Line::from_bytes(old_content).len();
        let options = if force_whole_file_replace || branch_line_divergence {
            atomic_core::record::workflow::RecordingOptions::new().force_whole_file_replace(true)
        } else {
            atomic_core::record::workflow::RecordingOptions::new()
        };
        record_modified_file(
            &memory_wc,
            &detected,
            old_content,
            None,
            &options,
            existing_trunk_id,
            if existing_branches.is_empty() {
                None
            } else {
                Some(existing_branches.as_slice())
            },
        )
        .map(|recorded| complete_semantic_replacement(recorded, &observed))
    }

    /// Record a foreign modification for a path that a Git-detected rename
    /// just moved: the new path has no inode binding in the graph yet (the
    /// move and the edit land in the same synthesized change), so the
    /// identity is anchored to the OLD path's stable inode and its CRDT
    /// trunk, while the record's path — and therefore the semantic FileOps —
    /// name the new path (CB-9C).
    ///
    /// # Errors
    ///
    /// Returns an error message when the old path has no inode binding or
    /// the recorded result is empty.
    pub fn record_foreign_renamed_file(
        &self,
        new_path: &str,
        old_path: &str,
        old_content: &[u8],
        new_content: &[u8],
        force_whole_file_replace: bool,
    ) -> Result<atomic_core::record::workflow::RecordedFile, String> {
        let (inode, position) = self
            .get_inode_and_position_on_view_excluding(old_path, &self.current_view, &[])
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("path '{old_path}' is absent from the selected closure"))?;
        let (existing_trunk_id, existing_branches, observed) =
            self.resolve_existing_semantic_state(old_path, inode, &self.current_view, &[])?;
        use atomic_core::record::workflow::{record_modified_file, DetectedFile};
        let memory_wc = atomic_core::output::memory::Memory::new();
        memory_wc.add_file(new_path, new_content);
        let mut detected = DetectedFile::modified(new_path);
        detected.inode = Some(inode);
        detected.position = Some(position);
        let options = if force_whole_file_replace {
            atomic_core::record::workflow::RecordingOptions::new().force_whole_file_replace(true)
        } else {
            atomic_core::record::workflow::RecordingOptions::new()
        };
        record_modified_file(
            &memory_wc,
            &detected,
            old_content,
            None,
            &options,
            existing_trunk_id,
            if existing_branches.is_empty() {
                None
            } else {
                Some(existing_branches.as_slice())
            },
        )
        .map(|recorded| complete_semantic_replacement(recorded, &observed))
    }

    /// The exact alive structural path claim for `path` on `view_name`, or
    /// `None` when the projection does not bind it. Refuses contested state:
    /// a path with zero causally maximal alive claims is unbound, and a path
    /// with several is ambiguous — a structural move must never route through
    /// an unproven source claim (the same authority rule the native
    /// `plan_file_moves` enforces via `source.claims.len() != 1`). The
    /// projected inode and position are returned alongside so callers can
    /// prove the claim binds the exact stable identity they resolved.
    ///
    /// # Errors
    ///
    /// Returns an error message when projection or claim reduction fails.
    pub fn resolve_single_path_claim(
        &self,
        view_name: &str,
        path: &str,
    ) -> Result<Option<(atomic_core::pristine::PathClaimId, Inode, Position<NodeId>)>, String> {
        self.resolve_single_path_claim_excluding(view_name, path, &[])
    }

    fn resolve_single_path_claim_excluding(
        &self,
        view_name: &str,
        path: &str,
        excluded: &[Hash],
    ) -> Result<Option<(atomic_core::pristine::PathClaimId, Inode, Position<NodeId>)>, String> {
        use atomic_core::pristine::ViewTxnT;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| format!("cannot open pristine: {e}"))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| format!("cannot read view: {e}"))?
            .ok_or_else(|| format!("view '{view_name}' does not exist"))?;
        let graph_visibility = super::filter::assembly_visibility_excluding(&txn, &view, excluded)
            .map_err(|e| format!("cannot build view visibility: {e}"))?;
        let projection = self
            .project_tree_for_visibility(&txn, &graph_visibility)
            .map_err(|e| format!("cannot project path claims: {e}"))?;
        let Some(claim) = projection.present_metadata.get(path) else {
            return Ok(None);
        };
        let mut claims = claim.claims.clone();
        if claims.len() != 1 {
            return Err(format!(
                "path '{path}' has {} causally maximal structural claims; refusing to \
                 route a structural move through a contested source claim",
                claims.len()
            ));
        }
        Ok(claims
            .pop()
            .map(|single| (single, claim.inode, claim.position)))
    }

    /// Record a foreign Git-detected rename through the native identity-
    /// preserving move pipeline (review CB-9C R2): the OLD path's stable
    /// inode, position, CRDT trunk and alive branches are resolved first and
    /// the exact source claim is validated, so the synthesized change carries
    /// a real semantic `TrunkOp::Move` (plus any destination line/token
    /// edits beneath it) — not merely a structural graph rename.
    ///
    /// The semantic trunk follows the inode through the move: it is resolved
    /// by the OLD path's inode, never by the new path (which cannot have a
    /// trunk yet). Destination bytes ride the same
    /// `record_moved_file` → `record_modified_file` pipeline an ordinary edit
    /// uses, so a pure move emits the semantic move alone and a move+edit
    /// emits the move plus the regenerated line ops.
    ///
    /// # Errors
    ///
    /// Returns an error message when the old path has no inode binding, no
    /// CRDT trunk, no uncontested source claim, or the recorded result is
    /// empty.
    #[allow(clippy::too_many_arguments)]
    pub fn record_foreign_moved_file(
        &self,
        view_name: &str,
        new_path: &str,
        old_path: &str,
        old_content: &[u8],
        new_content: &[u8],
        force_whole_file_replace: bool,
    ) -> Result<atomic_core::record::workflow::RecordedFile, String> {
        self.record_foreign_moved_file_excluding(
            view_name,
            new_path,
            old_path,
            old_content,
            new_content,
            force_whole_file_replace,
            &[],
        )
    }

    /// Record a move against the same parent closure used by Git assembly.
    #[allow(clippy::too_many_arguments)]
    pub fn record_foreign_moved_file_excluding(
        &self,
        view_name: &str,
        new_path: &str,
        old_path: &str,
        old_content: &[u8],
        new_content: &[u8],
        force_whole_file_replace: bool,
        excluded: &[Hash],
    ) -> Result<atomic_core::record::workflow::RecordedFile, String> {
        use atomic_core::record::workflow::{record_moved_file, DetectedFile, RecordingOptions};
        let (source, inode, position) = self
            .resolve_single_path_claim_excluding(view_name, old_path, excluded)?
            .ok_or_else(|| {
                format!("move source '{old_path}' has no alive path claim on view '{view_name}'")
            })?;
        let (existing_trunk_id, existing_branches, observed) =
            self.resolve_existing_semantic_state(old_path, inode, view_name, excluded)?;
        let existing_trunk_id = existing_trunk_id.ok_or_else(|| {
            format!("path '{old_path}' has no CRDT trunk for its canonical inode")
        })?;

        let memory_wc = atomic_core::output::memory::Memory::new();
        memory_wc.add_file(new_path, new_content);
        let detected = DetectedFile::moved(old_path, new_path)
            .with_inode(inode)
            .with_position(position);
        let options = if force_whole_file_replace {
            RecordingOptions::new().force_whole_file_replace(true)
        } else {
            RecordingOptions::new()
        };
        let recorded = record_moved_file(
            &memory_wc,
            &detected,
            old_content,
            None,
            &options,
            inode,
            position,
            source,
            existing_trunk_id,
            if existing_branches.is_empty() {
                None
            } else {
                Some(existing_branches.as_slice())
            },
        )
        .map_err(|message| format!("cannot record moved path '{new_path}': {message}"))?;
        if recorded.crdt_ops().is_none() {
            return Err(format!(
                "cannot record moved path '{new_path}': the native move pipeline produced \
                 no semantic FileOps"
            ));
        }
        Ok(complete_semantic_replacement(recorded, &observed))
    }

    /// Record a foreign Git deletion bound to the ACTUAL deleted trunk
    /// (review CB-9C R2, re-review EYL): the canonical delete record's
    /// semantic ops use a placeholder trunk that resolves to the deleting
    /// change, so `update_trunk_state` tombstoned nobody and the moved-to or
    /// previously-created trunk row stayed Alive after the graph deletion.
    /// This method resolves the OLD path's real CRDT trunk and alive branches
    /// first, then emits `TrunkOp::Delete` on that trunk with line deletes
    /// bound to the real branches — the semantic lifecycle matches the graph
    /// deletion on the trunk that actually exists.
    ///
    /// # Errors
    ///
    /// Returns an error message when the path has no inode binding, no CRDT
    /// trunk, or the canonical FileDel record is empty.
    pub fn record_foreign_deleted_file(
        &self,
        path: &str,
    ) -> Result<atomic_core::record::workflow::RecordedFile, String> {
        self.record_foreign_deleted_file_on_view_excluding(path, &self.current_view, &[])
    }

    /// Delete the identity visible in the requested assembly closure.
    pub fn record_foreign_deleted_file_on_view_excluding(
        &self,
        path: &str,
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<atomic_core::record::workflow::RecordedFile, String> {
        use atomic_core::change::FileOps;
        let (inode, position) = self
            .get_inode_and_position_on_view_excluding(path, view_name, excluded)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("path '{path}' is absent from view '{view_name}'"))?;
        let (existing_trunk_id, _, observed) =
            self.resolve_existing_semantic_state(path, inode, view_name, excluded)?;
        let existing_trunk_id = existing_trunk_id.ok_or_else(|| {
            format!(
                "path '{path}' has no CRDT trunk; refusing a semantic deletion without \
                     an existing semantic identity"
            )
        })?;

        let mut detected = atomic_core::record::workflow::DetectedFile::deleted(path);
        detected.inode = Some(inode);
        detected.position = Some(position);
        let core_options = atomic_core::record::workflow::RecordingOptions::new();
        let mut recorded =
            atomic_core::record::workflow::record_deleted_file(&detected, &core_options)?;
        if recorded.is_empty() {
            return Err(format!(
                "cannot import deletion '{path}': canonical FileDel is empty"
            ));
        }
        // Deletion dominates every line observed in this file's closure,
        // including lines concurrently restored in the ambient table.
        let mut ops = FileOps::delete(existing_trunk_id, path.to_string());
        for branch in observed {
            ops.add_line_op(atomic_core::change::LineOps::delete_empty(branch));
        }
        recorded.set_crdt_ops(ops);
        Ok(recorded)
    }

    /// Whether `path` carries persisted conflict state on `view_name` (review
    /// CB-11A). A conflicted path's rendered bytes are a conflict projection
    /// (marker lines with side provenance interleaved), not a plain file
    /// rendering: a positional diff against it cannot be mapped onto the
    /// graph's per-line vertices, so foreign deltas for such paths must be
    /// recorded as whole-file replacements instead.
    pub fn path_has_persisted_conflict(
        &self,
        path: &str,
        view_name: &str,
    ) -> Result<bool, RepositoryError> {
        use atomic_core::pristine::{TreeTxnT, ViewTxnT};
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let Some(inode) = txn
            .get_inode(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(false);
        };
        let conflicts = txn
            .get_conflicts(view.id, inode.get())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(!conflicts.is_empty())
    }

    /// Resolve the existing CRDT semantic state of a tracked path: the
    /// canonical trunk, alive branches in closure-local file order, and all
    /// observed branch identities. The latter is the deletion domain of a
    /// whole-file resolution; it must not depend on ambient lifecycle rows.
    /// Errors fail closed.
    fn resolve_existing_semantic_state(
        &self,
        path: &str,
        inode: atomic_core::types::Inode,
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<ExistingSemanticState, String> {
        use atomic_core::crdt::tables::decode_trunk_id;
        use atomic_core::pristine::CrdtTxnT;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| format!("cannot open pristine: {e}"))?;
        // The inode's canonical FileAdd fixes the trunk identity. Both path
        // and inode semantic indexes are derived and may need rebuilding.
        let position = txn
            .inode_position(inode)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("inode for '{path}' has no graph position"))?;
        let hash = txn
            .get_external(position.change)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("inode for '{path}' has no introducing change"))?;
        let creator = self.load_change(&hash).map_err(|e| e.to_string())?;
        let mut trunks = std::collections::HashSet::new();
        for hunk in creator.hunks() {
            if let atomic_core::change::GraphOp::FileAdd {
                add_inode,
                path: created_path,
                ..
            } = hunk
            {
                if add_inode.start == position.pos {
                    for ops in creator.file_ops() {
                        if ops.path() == created_path
                            && matches!(
                                ops.trunk_op(),
                                Some(atomic_core::crdt::TrunkOp::Create { .. })
                            )
                        {
                            let raw = ops.trunk_id();
                            trunks.insert(if raw.change_id().is_root() {
                                atomic_core::crdt::TrunkId::new(position.change, raw.file_idx())
                            } else {
                                raw
                            });
                        }
                    }
                }
            }
        }
        if trunks.len() > 1 {
            return Err(format!("multiple semantic identities for '{path}'"));
        }
        let existing_trunk_id = trunks.into_iter().next();
        let Some(trunk) = existing_trunk_id else {
            return Ok((None, Vec::new(), Vec::new()));
        };
        let indexed = txn
            .get_crdt_inode_trunk(inode.get())
            .map_err(|e| e.to_string())?
            .map(|key| decode_trunk_id(&key));
        if indexed != Some(trunk) {
            return Err(format!("semantic inode index for '{path}' disagrees with its canonical FileAdd; rebuild semantic indexes"));
        }
        let view = txn
            .get_view(view_name)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("view '{view_name}' does not exist"))?;
        let visibility = super::filter::assembly_visibility_excluding(&txn, &view, excluded)
            .map_err(|e| e.to_string())?;
        let mut alive = std::collections::HashMap::new();
        let mut after = std::collections::BTreeMap::new();
        for change_id in visibility
            .iter_dependency_first()
            .copied()
            .filter(|id| !id.is_root())
        {
            let hash = txn
                .get_external(change_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("closure change {change_id} has no hash"))?;
            let change = self.load_change(&hash).map_err(|e| e.to_string())?;
            for ops in change.file_ops() {
                let raw = ops.trunk_id();
                let resolved = if raw.change_id().is_root() {
                    atomic_core::crdt::TrunkId::new(change_id, raw.file_idx())
                } else {
                    raw
                };
                if resolved != trunk {
                    continue;
                }
                for line in ops.line_ops() {
                    let raw = line.branch_id();
                    let branch = if raw.change_id().is_root() {
                        atomic_core::crdt::BranchId::new(change_id, raw.branch_idx())
                    } else {
                        raw
                    };
                    use atomic_core::crdt::BranchOp;
                    let resolve = |id: atomic_core::crdt::BranchId| {
                        if id.change_id().is_root() {
                            atomic_core::crdt::BranchId::new(change_id, id.branch_idx())
                        } else {
                            id
                        }
                    };
                    match line.operation() {
                        BranchOp::Insert {
                            after: predecessor, ..
                        } => {
                            alive.insert(branch, true);
                            after.insert(branch, predecessor.map(resolve));
                        }
                        BranchOp::Modify { .. } | BranchOp::Restore { .. } => {
                            alive.insert(branch, true);
                        }
                        BranchOp::Delete { .. } => {
                            alive.insert(branch, false);
                        }
                        BranchOp::Reparent { new_after, .. } => {
                            after.insert(branch, new_after.map(resolve));
                        }
                    }
                }
            }
        }
        let observed = semantic_branch_order(&after)?;
        let existing_branches = observed
            .iter()
            .copied()
            .filter(|branch| alive.get(branch) == Some(&true))
            .collect();
        Ok((existing_trunk_id, existing_branches, observed))
    }

    /// This is the main entry point for creating a change from working copy
    /// modifications. It detects changes, creates hunks, globalizes positions,
    /// and assembles a complete change.
    ///
    /// # Arguments
    ///
    /// * `working_copy` - The validated physical working-copy identity
    /// * `header` - The change header (message, author, etc.)
    /// * `options` - Options controlling recording behavior
    ///
    /// # Returns
    ///
    /// A `RecordOutcome` containing the recorded change, hash, and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No changes are detected (working copy is clean)
    /// - A file cannot be read
    /// - Globalization fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::{Repository, RecordOptions};
    /// use atomic_core::change::{Author, ChangeHeader};
    ///
    /// let repo = Repository::open(".")?;
    ///
    /// let header = ChangeHeader::builder()
    ///     .message("Add new feature")
    ///     .author(Author::new("Alice", Some("alice@example.com")))
    ///     .build();
    ///
    /// let result = repo.record(working_copy, header, RecordOptions::default())?;
    /// println!("Created change: {}", result.hash().to_base32());
    /// ```
    pub fn record(
        &self,
        working_copy: WorkingCopyId,
        header: ChangeHeader,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        self.record_with_lifecycle(
            working_copy,
            header,
            options,
            super::snapshot::RecordLifecycle::Durable,
        )
    }

    pub(super) fn record_with_lifecycle(
        &self,
        working_copy: WorkingCopyId,
        header: ChangeHeader,
        options: RecordOptions,
        lifecycle: super::snapshot::RecordLifecycle,
    ) -> Result<RecordOutcome, RecordError> {
        self.validate_working_copy(working_copy)
            .map_err(RecordError::Repository)?;
        let desired_view = self
            .desired_view_name(working_copy)
            .map_err(RecordError::Repository)?;
        let effective_view = options
            .get_view()
            .map(str::to_owned)
            .unwrap_or_else(|| desired_view.clone());
        if effective_view != desired_view {
            return Err(RecordError::Repository(RepositoryError::InvalidOperation {
                message: format!(
                    "record view '{}' does not match working copy {} desired view '{}'",
                    effective_view, working_copy, desired_view
                ),
            }));
        }

        let target_view = lifecycle.target_view(&desired_view).to_string();

        // Scoped stale-conflict cleanup: when the caller explicitly names paths
        // and opts into conflict-marker content, reconcile persisted conflict
        // rows that no longer describe a real graph conflict. This runs before
        // any long-lived read transaction is opened so the journaled mutation
        // can take its own write boundary, and it only touches the named paths.
        let mut conflict_cleanup: Option<ConflictReconcileOutcome> = None;
        if options.get_allow_conflict_markers() && !options.all() && !options.get_paths().is_empty()
        {
            if let Some(outcome) = self
                .reconcile_explicit_conflicts(working_copy, options.get_paths())
                .map_err(RecordError::Repository)?
            {
                conflict_cleanup = Some(outcome);
            }
        }

        let trace_record = std::env::var_os("ATOMIC_TRACE_RECORD").is_some();
        use atomic_core::output::Memory;
        use atomic_core::record::workflow::{
            assemble_change, record_added_file, record_deleted_file, record_modified_file,
            record_moved_file, record_undeleted_file, DetectedFile, RecordedFile,
        };

        // Build the final header (may get message from options).
        let final_header = build_header(header, &options);

        // A remote sandbox's cache takes the rows it is about to read.
        self.hydrate_remote_sandbox()
            .map_err(RecordError::Repository)?;

        // Get repository status to find modified files
        let status_t0 = std::time::Instant::now();
        let mut status_options = StatusOptions::default();
        if !options.all() {
            for path in options.get_paths() {
                status_options = status_options.filter_path(std::path::PathBuf::from(path));
            }
        }
        let status = self
            .status_for_record(
                working_copy,
                status_options,
                options.get_detect_raw_renames(),
                options.get_include_untracked(),
            )
            .map_err(RecordError::Repository)?;
        if trace_record {
            eprintln!(
                "[record] status: {:.1}ms ({} entries)",
                status_t0.elapsed().as_secs_f64() * 1000.0,
                status.entries().len(),
            );
        }

        log::debug!(
            "record: status returned {} entries (modified={}, added={}, deleted={}, clean={}, untracked={})",
            status.entries().len(),
            status.modified_count(),
            status.added_count(),
            status.deleted_count(),
            status.entries().iter().filter(|e| e.status() == FileStatus::Clean).count(),
            status.untracked_count(),
        );

        // Refuse to record files that still contain conflict markers, so an
        // unresolved merge is never baked into history. This runs over the
        // raw status (before the recordable filter) so it also catches files
        // reported as Conflicted. Overridable via
        // `RecordOptions::allow_conflict_markers`.
        if !options.get_allow_conflict_markers() {
            for entry in status.entries() {
                let is_directory = entry.details().map(|d| d == "directory").unwrap_or(false);
                if is_directory {
                    continue;
                }
                if !matches!(
                    entry.status(),
                    FileStatus::Added | FileStatus::Modified | FileStatus::Conflicted
                ) {
                    continue;
                }
                let path = entry.path().to_string_lossy().to_string();
                if !options.should_include(&path) {
                    continue;
                }
                let full_path = self.root.join(&path);
                if let Ok(content) = std::fs::read(&full_path) {
                    if let Some(line) = super::materialize::first_conflict_marker_line(&content) {
                        return Err(RecordError::ConflictMarkersPresent { path, line });
                    }
                }
            }
        }

        // Move classification runs after the graph-backed path projection is
        // available. This keeps exact source claims authoritative and ensures
        // content equality/similarity is retained only as advisory evidence.

        // Statistics tracking
        let mut stats = RecordStats::new();
        let mut recorded_files: Vec<RecordedFile> = Vec::new();
        let mut recorded_paths: Vec<String> = Vec::new();
        let mut deleted_paths: Vec<String> = Vec::new();
        let mut skipped_paths: Vec<String> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();

        let core_options = options.to_core_options();
        let opaque_core_options = core_options
            .clone()
            .max_file_size(usize::MAX)
            .skip_binary(false);
        let content_filter = crate::content_filter::GitAttributesFilter::for_repository(&self.root);
        use crate::content_filter::ContentFilter;

        // Create a memory working copy for the recording workflow
        let memory_wc = Memory::new();

        let record_t0 = std::time::Instant::now();

        use super::content::retrieve_content_with_filter_fast_with_fork_info;

        // Build and validate the target view's graph visibility once for all
        // record traversal, then share O(1) clones across files and assembly.
        let shared_txn = self
            .pristine
            .read_txn()
            .map_err(|e| RecordError::Database(e.to_string()))?;
        let view = shared_txn
            .get_view(&effective_view)
            .map_err(|e| RecordError::Database(e.to_string()))?
            .ok_or_else(|| {
                RecordError::Repository(RepositoryError::ViewNotFound {
                    name: effective_view.clone(),
                })
            })?;
        let shared_graph_visibility =
            graph_visibility_closure(&shared_txn, &view).map_err(RecordError::Repository)?;
        let tree_projection = self
            .project_tree_for_visibility(&shared_txn, &shared_graph_visibility)
            .map_err(RecordError::Repository)?;
        let projected_present = tree_projection.present_metadata;
        let projected_absent: std::collections::HashMap<
            String,
            super::deferred_tree::ProjectedAbsent,
        > = tree_projection.absent_metadata;
        let projected_name_conflicts = tree_projection.name_conflicts;

        // Alongside visibility, collect paths with a PERSISTED
        // conflict on this view (the raw CONFLICTS table, deliberately NOT the
        // marker-gated `list_conflicts`). At resolution time the user has
        // already edited the markers out of the file, so a Modified record for
        // one of these paths is a conflict RESOLUTION — the fork structure the
        // retrieval sees is the surfaced conflict being superseded, which is
        // the expected workflow, not an anomaly. Used below to route the
        // fork-structure log to debug for resolutions and keep WARN for
        // genuinely unexpected fork structure.
        let mut conflicted_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        if let Ok(conflicts) = shared_txn.iter_conflicts(view.id) {
            for (_inode, records) in conflicts {
                for conflict in records {
                    conflicted_paths.insert(conflict.path.clone());
                }
            }
        }
        // Dev #203: a NodeId-set change filter for the supersession-aware
        // aliveness probes below, derived from the same graph visibility the
        // projection uses.
        let shared_change_filter: std::collections::HashSet<NodeId> = shared_graph_visibility
            .iter_dependency_first()
            .copied()
            .collect();

        let shared_cached_txn =
            CachedGraphTxn::new(&shared_txn).map_err(|e| RecordError::Database(e.to_string()))?;
        let move_planning = plan_file_moves(
            working_copy,
            &shared_txn,
            &shared_cached_txn,
            &self.change_store,
            &shared_graph_visibility,
            status.entries(),
            &projected_present,
            &projected_absent,
            &self.root,
            &content_filter,
            options.get_detect_raw_renames(),
        )?;
        let FileMovePlanning {
            moves: planned_moves,
            evidence: mut move_evidence,
            unresolved_destinations,
            fresh_additions,
        } = move_planning;
        let renamed_from: std::collections::HashSet<String> = planned_moves
            .iter()
            .map(|planned| planned.old_path.clone())
            .collect();
        let renamed_to: std::collections::HashSet<String> = planned_moves
            .iter()
            .map(|planned| planned.new_path.clone())
            .collect();
        let move_source_paths = renamed_from.clone();

        // Repository-owned `--all` handling runs after move classification, so
        // adding an untracked destination can no longer erase rename evidence.
        // Ambiguous heuristic destinations are also included to make the
        // conservative fallback an explicit delete-plus-add change.
        let mut remapped_entries: Vec<FileStatusEntry> = status
            .entries()
            .iter()
            .map(|entry| {
                let path = entry.path().to_string_lossy();
                let remapped_status = if entry.status() == FileStatus::Conflicted
                    && options.get_allow_conflict_markers()
                {
                    Some(FileStatus::Modified)
                } else if entry.status() == FileStatus::Untracked
                    && (options.get_include_untracked()
                        || unresolved_destinations.contains(path.as_ref()))
                {
                    Some(FileStatus::Added)
                } else {
                    None
                };
                let Some(remapped_status) = remapped_status else {
                    return entry.clone();
                };
                let mut remapped =
                    FileStatusEntry::new(entry.path().to_path_buf(), remapped_status);
                if let Some(inode) = entry.inode() {
                    remapped.set_inode(inode);
                }
                remapped
            })
            .collect();
        remapped_entries.sort_by(|left, right| left.path().cmp(right.path()));

        // A projected name conflict (two or more live claimants) is surfaced by
        // `status` as `Conflicted`, and the ordinary recordable filter then
        // excludes it — including when the working tree already holds exactly
        // one incarnation's bytes and no conflict row was ever persisted. That
        // silently preserves the ambiguity forever. For an explicit ask
        // (`--all`, or a named path) classify every resolvable *file* name
        // conflict as a Modified candidate so the normal
        // `prepare_name_resolution` path can supersede the losing claims inside
        // a recorded change.
        //
        // An exact content selection retains that side's identity. An explicit
        // edited resolution records the new content against a canonically
        // ordered claimant and supersedes the other name claims. Observation
        // never resolves anything; rename/directory conflicts use their own
        // flows, and preserve-content requests retain their strict contract.
        // Review ::15 blocker support (user decision "Preserve current
        // files"): paths in `resolve_name_conflicts` are EXPLICIT resolution
        // targets — they qualify without --all and may carry MULTIPLE
        // byte-equal claimants (the preserve-mode canonical selection in
        // `prepare_name_resolution` handles that case; a zero-match path is
        // still refused — unrecorded working content is never silently
        // chosen).
        let explicit_resolution_targets: std::collections::HashSet<String> = options
            .get_resolve_name_conflicts()
            .iter()
            .map(|path| path.replace('\\', "/"))
            .collect();
        let name_resolvable: std::collections::HashSet<String> = projected_name_conflicts
            .iter()
            .filter(|(path, conflict)| {
                if conflict.is_rename_conflict() {
                    return false;
                }
                let explicit = explicit_resolution_targets.contains(path.as_str());
                if !explicit && !options.all() && options.get_paths().is_empty() {
                    return false;
                }
                if !explicit && !options.should_include(path) {
                    return false;
                }
                let sides = conflict.sides_at_path(path);
                sides.len() >= 2 && sides.iter().all(|side| !side.is_directory())
            })
            .map(|(path, _)| path.clone())
            .collect();
        if !name_resolvable.is_empty() {
            remapped_entries.retain(|entry| {
                !(entry.status() == FileStatus::Conflicted
                    && name_resolvable.contains(entry.path().to_string_lossy().as_ref()))
            });
            let present: std::collections::HashSet<String> = remapped_entries
                .iter()
                .map(|entry| entry.path().to_string_lossy().into_owned())
                .collect();
            for path in &name_resolvable {
                if present.contains(path) {
                    continue;
                }
                let mut entry =
                    FileStatusEntry::new(std::path::PathBuf::from(path), FileStatus::Modified);
                if let Some(side) = projected_name_conflicts
                    .get(path)
                    .and_then(|conflict| conflict.sides_at_path(path).first().copied())
                {
                    entry.set_inode(side.inode);
                }
                remapped_entries.push(entry);
            }
            remapped_entries.sort_by(|left, right| left.path().cmp(right.path()));
        }

        let files_to_record = filter_files(&remapped_entries, &options);

        log::debug!(
            "record: filter_files returned {} recordable files and {} moves",
            files_to_record.len(),
            planned_moves.len(),
        );
        for file in &files_to_record {
            log::debug!("record:   {:?} {}", file.status(), file.path().display());
        }

        if files_to_record.is_empty() && planned_moves.is_empty() {
            if let Some(cleanup) = conflict_cleanup.take() {
                return Ok(conflict_cleanup_outcome(final_header.clone(), cleanup));
            }
            return Err(RecordError::NothingToRecord);
        }

        #[derive(Clone)]
        struct PendingAttribute {
            path: String,
            position: Option<Position<NodeId>>,
            trunk: Option<atomic_core::crdt::TrunkId>,
            value: atomic_core::change::InodeAttr,
            dependencies: Vec<Hash>,
        }

        let visible_attribute_changes: std::collections::HashSet<NodeId> = shared_graph_visibility
            .iter_dependency_first()
            .copied()
            .collect();
        let mut pending_attributes = Vec::<PendingAttribute>::new();
        // A move retains the graph identity validated by plan_file_moves. Its
        // destination can be untracked (and absent from files_to_record without
        // --all), so collect it from the move plan exactly once, never as a new
        // FileAdd target. This also records simultaneous mode/kind changes.
        let attribute_targets = files_to_record
            .iter()
            .filter_map(|entry| {
                let path = entry.path().to_string_lossy().to_string();
                if entry.status() == FileStatus::Deleted
                    || entry.details() == Some("directory")
                    || renamed_from.contains(&path)
                    || renamed_to.contains(&path)
                {
                    None
                } else {
                    Some((path, entry.inode(), None))
                }
            })
            .chain(planned_moves.iter().map(|planned| {
                (
                    planned.new_path.clone(),
                    Some(planned.inode),
                    Some(planned.claimant),
                )
            }));
        for (path, inode, move_position) in attribute_targets {
            // A name-conflict resolution selects one existing claimant later in
            // the content phase. There is no new FileAdd inode to receive attrs;
            // the selected claimant retains its causal mode/kind state.
            if projected_name_conflicts.contains_key(&path) {
                continue;
            }
            let attrs = super::attributes::working_inode_attrs(&self.root.join(&path))
                .map_err(RecordError::Repository)?;
            let position = match move_position {
                Some(position) => Some(position),
                None => inode
                    .map(|inode| shared_txn.inode_position(inode))
                    .transpose()
                    .map_err(|error| RecordError::Database(error.to_string()))?
                    .flatten(),
            };
            use atomic_core::crdt::tables::decode_trunk_id;
            use atomic_core::pristine::CrdtTxnT;
            let trunk = inode
                .map(|inode| shared_txn.get_crdt_inode_trunk(inode.get()))
                .transpose()
                .map_err(|error| RecordError::Database(error.to_string()))?
                .flatten()
                .map(|key| decode_trunk_id(&key));
            let projected = position
                .map(|position| {
                    atomic_core::output::project_inode_attributes(
                        &shared_txn,
                        position,
                        &visible_attribute_changes,
                    )
                    .map_err(|error| RecordError::Database(error.to_string()))
                })
                .transpose()?;
            if projected
                .as_ref()
                .is_some_and(|value| value.is_conflicted())
            {
                return Err(RecordError::Database(format!(
                    "cannot record '{}' while its inode attributes conflict",
                    path
                )));
            }
            let expected = projected
                .map(|value| value.materialization)
                .unwrap_or_default();
            let attribute_dependencies = |value: atomic_core::change::InodeAttr| {
                let Some(position) = position else {
                    return Ok(Vec::new());
                };
                let state = atomic_core::pristine::InodeAttrTxnT::resolve_inode_attr(
                    &shared_txn,
                    position,
                    value.name(),
                    &visible_attribute_changes,
                )
                .map_err(|error| RecordError::Database(error.to_string()))?;
                state
                    .events()
                    .iter()
                    .map(|event| {
                        shared_txn
                            .get_external(event.introduced_by)
                            .map_err(|error| RecordError::Database(error.to_string()))?
                            .ok_or_else(|| {
                                RecordError::Database(format!(
                                    "attribute event change {:?} has no external hash",
                                    event.introduced_by
                                ))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()
            };
            if position.is_none() || expected.mode != attrs.mode {
                let value = atomic_core::change::InodeAttr::Mode(attrs.mode);
                pending_attributes.push(PendingAttribute {
                    path: path.clone(),
                    position,
                    trunk,
                    value,
                    dependencies: attribute_dependencies(value)?,
                });
            }
            if position.is_none() || expected.kind != attrs.kind {
                let value = atomic_core::change::InodeAttr::Kind(attrs.kind);
                pending_attributes.push(PendingAttribute {
                    path,
                    position,
                    trunk,
                    value,
                    dependencies: attribute_dependencies(value)?,
                });
            }
        }

        for planned in planned_moves {
            let raw = crate::content_filter::read_working_bytes(&self.root.join(&planned.new_path))
                .map_err(|error| {
                    RecordError::Database(format!(
                        "failed to read moved path '{}': {error}",
                        planned.new_path
                    ))
                })?;
            let filtered = content_filter
                .clean(std::path::Path::new(&planned.new_path), &raw)
                .map_err(|error| RecordError::Database(error.to_string()))?;
            for warning in &filtered.warnings {
                log::warn!("record: {warning}");
            }
            let opaque = content_filter.is_git_tracked(std::path::Path::new(&planned.new_path))
                && (filtered.bytes.len() as u64 > options.max_file_size()
                    || crate::content_filter::looks_binary(&filtered.bytes));
            let moved_working_copy = Memory::new();
            moved_working_copy.add_file(&planned.new_path, &filtered.bytes);
            let (trunk, branches) =
                existing_crdt_identity(&shared_txn, planned.inode, &shared_graph_visibility)?;
            let detected = DetectedFile::moved(&planned.old_path, &planned.new_path)
                .with_inode(planned.inode)
                .with_position(planned.claimant);
            let mut recorded = record_moved_file(
                &moved_working_copy,
                &detected,
                &planned.old_content,
                None,
                if opaque {
                    &opaque_core_options
                } else {
                    &core_options
                },
                planned.inode,
                planned.claimant,
                planned.source,
                trunk,
                (!branches.is_empty()).then_some(branches.as_slice()),
            )
            .map_err(RecordError::Database)?;
            if opaque {
                recorded.set_opaque_generated(true);
                log::warn!(
                    "record: Git-tracked '{}' is large or binary; preserving exact repository bytes as opaque content",
                    planned.new_path
                );
            }

            stats.files_recorded += 1;
            stats.hunks_created += recorded.hunk_count() + 1;
            stats.vertices_added += 1;
            stats.edges_modified += 1;
            for hunk in recorded.hunks() {
                use atomic_core::record::workflow::graph_op::BuiltHunkKind;
                match hunk.kind {
                    BuiltHunkKind::Insert => stats.vertices_added += 1,
                    BuiltHunkKind::Replace => {
                        stats.vertices_added += 1;
                        stats.edges_modified += 1;
                    }
                    BuiltHunkKind::Delete => stats.edges_modified += 1,
                }
                stats.content_bytes += hunk.content_len();
            }
            if let Some(crdt_stats) = recorded.crdt_stats() {
                stats.lines_added += crdt_stats.lines_added;
                stats.lines_deleted += crdt_stats.lines_deleted;
                stats.lines_modified += crdt_stats.lines_modified;
                stats.tokens_added += crdt_stats.tokens_added;
                stats.tokens_deleted += crdt_stats.tokens_deleted;
                stats.tokens_replaced += crdt_stats.tokens_replaced;
            }
            recorded_paths.push(planned.new_path);
            recorded_files.push(recorded);
        }

        // TREE selects just one inode per path. Recover competing live
        // identities exactly as materialization does before diffing that inode.
        let selected_paths: HashSet<&str> = files_to_record
            .iter()
            .filter_map(|e| e.path().to_str())
            .collect();
        let name_conflicts = super::materialize::collect_name_conflicts(
            &shared_txn,
            &self.change_store,
            &selected_paths,
            &shared_change_filter,
        )?;

        // A namespace conflict has several legitimate identities. TREE's
        // occupant is an index artifact, not a choice of which file to keep.
        // Prefer an identity whose bytes match the user's resolution. Break
        // ties (including newly edited content) by external graph identity,
        // never repository-local allocation or cross-view index update order.
        let mut selected_name_inodes = std::collections::HashMap::new();
        for entry in &files_to_record {
            if entry.status() != FileStatus::Modified {
                continue;
            }
            let path = entry.path().to_string_lossy();
            let Some(sides) = name_conflicts.get(path.as_ref()) else {
                continue;
            };
            let working = std::fs::read(self.root.join(path.as_ref())).map_err(|e| {
                RecordError::Database(format!("cannot resolve name conflict for {path}: {e}"))
            })?;
            let mut candidates = Vec::new();
            for &(inode, position) in sides {
                let options = atomic_core::output::RetrieveOptions::new()
                    .with_change_filter(shared_change_filter.clone());
                let (content, _) = retrieve_content_with_filter_fast_with_fork_info(
                    &shared_cached_txn,
                    &self.change_store,
                    inode,
                    position,
                    options,
                )
                .map_err(|e| {
                    RecordError::Database(format!("cannot resolve name conflict for {path}: {e}"))
                })?;
                let hash = shared_txn
                    .get_external(position.change)
                    .map_err(|e| RecordError::Database(e.to_string()))?
                    .ok_or_else(|| {
                        RecordError::Database(format!("{path}: missing identity hash"))
                    })?;
                candidates.push(((content != working, hash, position.pos), (inode, position)));
            }
            candidates.sort_by_key(|(key, _)| *key);
            if let Some((_, identity)) = candidates.first() {
                selected_name_inodes.insert(path.into_owned(), *identity);
            }
        }

        if trace_record {
            eprintln!(
                "[record] change filter: {} visible changes",
                shared_graph_visibility.len()
            );
        }

        // Phase 1: process Added/Deleted sequentially, collect Modified for parallel.
        let mut modified_work: Vec<(String, std::path::PathBuf, std::time::Instant)> = Vec::new();

        for entry in &files_to_record {
            stats.files_processed += 1;

            let path = entry.path().to_string_lossy().to_string();
            let full_path = self.root.join(&path);
            let file_t0 = std::time::Instant::now();

            // Skip the old side of a detected rename: it was reclassified as a
            // FileMove above, so recording it as a Deleted here would emit a
            // spurious FileDel and drop the inode.
            if renamed_from.contains(&path) || renamed_to.contains(&path) {
                continue;
            }

            // Check if this is a directory (from the details field)
            let is_directory = entry.details().map(|d| d == "directory").unwrap_or(false);

            match entry.status() {
                FileStatus::Added if is_directory => {
                    // Handle added directory - create DirAdd graph_op
                    // For now, directories are tracked but their hunks will be
                    // generated during globalization. We just need to record
                    // that this directory was added.
                    stats.directories_recorded += 1;
                    stats.vertices_added += 2; // name span + inode span
                    if std::fs::read_dir(&full_path)
                        .map(|mut entries| entries.next().is_none())
                        .unwrap_or(false)
                    {
                        move_evidence
                            .insert_loss(crate::record::LossNote::empty_directory(path.clone()));
                    }
                    recorded_paths.push(format!("{}/ (directory)", path));

                    let recorded = if let Some(absent) = projected_absent.get(&path) {
                        if !absent.directory {
                            errors.push((
                                path.clone(),
                                "Projected undelete changes file kind to directory".to_string(),
                            ));
                            stats.errors += 1;
                            continue;
                        }
                        RecordedFile::new_undeleted_directory(
                            &path,
                            absent.inode,
                            absent.position,
                            absent.deleted_by.clone(),
                        )
                    } else {
                        // The actual GraphOp::DirAdd is created during globalization.
                        RecordedFile::new_directory(&path)
                    };
                    recorded_files.push(recorded);
                }

                FileStatus::Added => {
                    // Read file content
                    log::debug!(
                        "record: processing Added file '{}' (full_path={})",
                        path,
                        full_path.display()
                    );
                    match crate::content_filter::read_working_bytes(&full_path) {
                        Ok(working_content) => {
                            let filtered = content_filter
                                .clean(std::path::Path::new(&path), &working_content)
                                .map_err(|error| RecordError::Database(error.to_string()))?;
                            for warning in &filtered.warnings {
                                log::warn!("record: {warning}");
                            }
                            let content = filtered.bytes;
                            log::debug!(
                                "record: read {} repository bytes from '{}'",
                                content.len(),
                                path
                            );
                            let git_tracked =
                                content_filter.is_git_tracked(std::path::Path::new(&path));
                            let opaque = git_tracked
                                && (content.len() as u64 > options.max_file_size()
                                    || crate::content_filter::looks_binary(&content));
                            if content.len() as u64 > options.max_file_size() && !opaque {
                                if options.skip_binary() {
                                    skipped_paths.push(path.clone());
                                    stats.files_skipped += 1;
                                    continue;
                                }
                                return Err(RecordError::FileTooLarge {
                                    path: path.clone(),
                                    size: content.len() as u64,
                                    limit: options.max_file_size(),
                                });
                            }

                            // Only repository bytes enter the recording workflow.
                            memory_wc.add_file(&path, &content);

                            let mut detected = DetectedFile::added(&path);
                            let recorded_result = if fresh_additions.contains(&path) {
                                record_added_file(
                                    &memory_wc,
                                    &detected,
                                    if opaque {
                                        &opaque_core_options
                                    } else {
                                        &core_options
                                    },
                                )
                            } else if let Some(absent) = projected_absent.get(&path) {
                                if absent.directory {
                                    Err("Projected undelete changes directory kind to file"
                                        .to_string())
                                } else {
                                    detected.inode = Some(absent.inode);
                                    detected.position = Some(absent.position);
                                    let baseline = visibility_before_deletions(
                                        &shared_txn,
                                        &absent.deleted_by,
                                    )?;
                                    let old_content =
                                        retrieve_content_with_filter_fast_with_fork_info(
                                            &shared_cached_txn,
                                            &self.change_store,
                                            absent.inode,
                                            absent.position,
                                            atomic_core::output::alive::RetrieveOptions::new()
                                                .with_graph_visibility(baseline.clone()),
                                        )
                                        .map_err(|error| {
                                            RecordError::Database(format!(
                                            "failed to retrieve pre-delete content for '{}': {}",
                                            path, error
                                        ))
                                        })?
                                        .0;
                                    let (trunk, branches) = existing_crdt_identity(
                                        &shared_txn,
                                        absent.inode,
                                        &baseline,
                                    )?;
                                    record_undeleted_file(
                                        &memory_wc,
                                        &detected,
                                        &absent.deleted_by,
                                        &old_content,
                                        None,
                                        if opaque {
                                            &opaque_core_options
                                        } else {
                                            &core_options
                                        },
                                        trunk,
                                        (!branches.is_empty()).then_some(branches.as_slice()),
                                    )
                                }
                            } else {
                                record_added_file(
                                    &memory_wc,
                                    &detected,
                                    if opaque {
                                        &opaque_core_options
                                    } else {
                                        &core_options
                                    },
                                )
                            };

                            let recorded_result = recorded_result.map(|mut recorded| {
                                if opaque {
                                    recorded.set_opaque_generated(true);
                                    log::warn!(
                                        "record: Git-tracked '{}' is large or binary; preserving exact repository bytes as opaque content",
                                        path
                                    );
                                }
                                recorded
                            });
                            match recorded_result {
                                Ok(recorded) => {
                                    log::debug!(
                                        "record: record_added_file '{}' returned: is_empty={} hunks={} content_len={}",
                                        path, recorded.is_empty(), recorded.hunk_count(), recorded.content_len()
                                    );
                                    if !recorded.is_empty() {
                                        stats.files_recorded += 1;
                                        stats.hunks_created += recorded.hunk_count();
                                        stats.content_bytes += recorded.content_len() as u64;
                                        if recorded.is_undelete() {
                                            stats.edges_modified += 1;
                                        } else {
                                            // FileAdd creates name, inode, and content vertices.
                                            stats.vertices_added += 3;
                                        }

                                        // Collect CRDT token-level statistics
                                        if let Some(crdt_stats) = recorded.crdt_stats() {
                                            stats.lines_added += crdt_stats.lines_added;
                                            stats.lines_deleted += crdt_stats.lines_deleted;
                                            stats.lines_modified += crdt_stats.lines_modified;
                                            stats.tokens_added += crdt_stats.tokens_added;
                                            stats.tokens_deleted += crdt_stats.tokens_deleted;
                                            stats.tokens_replaced += crdt_stats.tokens_replaced;
                                        }

                                        recorded_paths.push(path.clone());
                                        recorded_files.push(recorded);
                                    } else {
                                        log::debug!(
                                            "record: '{}' produced empty result, skipping",
                                            path
                                        );
                                        skipped_paths.push(path.clone());
                                        stats.files_skipped += 1;
                                    }
                                }
                                Err(e) => {
                                    log::debug!(
                                        "record: record_added_file '{}' error: {:?}",
                                        path,
                                        e
                                    );
                                    errors.push((path.clone(), format!("{:?}", e)));
                                    stats.errors += 1;
                                }
                            }
                        }
                        Err(e) => {
                            log::debug!("record: failed to read '{}': {}", path, e);
                            errors.push((path.clone(), e.to_string()));
                            stats.errors += 1;
                        }
                    }
                }

                FileStatus::Deleted if is_directory => {
                    // Handle deleted directory - create DirDel graph_op
                    // Look up the directory's inode
                    let txn = self
                        .pristine
                        .read_txn()
                        .map_err(|e| RecordError::Database(e.to_string()))?;

                    let inode = match txn.get_inode(&path) {
                        Ok(Some(inode)) => inode,
                        Ok(None) => {
                            errors.push((path.clone(), "Directory inode not found".to_string()));
                            stats.errors += 1;
                            continue;
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("Failed to get inode: {}", e)));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    // Verify it's actually a directory
                    if !txn.is_directory(inode).unwrap_or(false) {
                        errors.push((path.clone(), "Path is not a directory".to_string()));
                        stats.errors += 1;
                        continue;
                    }

                    // Get the position for this directory's inode
                    let position = match txn.inode_position(inode) {
                        Ok(Some(pos)) => pos,
                        Ok(None) => {
                            errors.push((path.clone(), "Directory position not found".to_string()));
                            stats.errors += 1;
                            continue;
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("Failed to get position: {}", e)));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    stats.directories_recorded += 1;
                    stats.edges_modified += 1; // deletion edge
                                               // Store the actual path for tree deletion, not the display format
                    deleted_paths.push(path.clone());

                    // Create a RecordedFile for the deleted directory with inode and position
                    let mut recorded = RecordedFile::new_deleted_directory(&path);
                    recorded.set_inode(inode);
                    recorded.set_position(position);
                    recorded_files.push(recorded);
                }

                FileStatus::Deleted => {
                    // For deleted files, we need to look up the inode and position
                    // from the pristine so that globalization can find the content
                    // vertices to mark as deleted.
                    let (file_inode, file_position) = {
                        let txn = self
                            .pristine
                            .read_txn()
                            .map_err(|e| RecordError::Database(e.to_string()))?;

                        // Get the inode for this path
                        let inode = match txn.get_inode(&path) {
                            Ok(Some(inode)) => inode,
                            Ok(None) => {
                                // No inode found - file was never recorded
                                errors.push((
                                    path.clone(),
                                    "File inode not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors.push((path.clone(), format!("Failed to get inode: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        // Get the graph position for this inode
                        let position = match txn.inode_position(inode) {
                            Ok(Some(pos)) => pos,
                            Ok(None) => {
                                errors.push((
                                    path.clone(),
                                    "File position not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors
                                    .push((path.clone(), format!("Failed to get position: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        (inode, position)
                    };

                    // Create a detected file descriptor for deletion with inode/position
                    let mut detected = DetectedFile::deleted(&path);
                    detected.inode = Some(file_inode);
                    detected.position = Some(file_position);

                    // Record deletion (no content needed)
                    match record_deleted_file(&detected, &core_options) {
                        Ok(recorded) => {
                            stats.files_recorded += 1;
                            stats.hunks_created += recorded.hunk_count();
                            // FileDel creates EdgeUpdate atoms to mark edges as deleted
                            stats.edges_modified += 1;

                            // Collect CRDT token-level statistics
                            if let Some(crdt_stats) = recorded.crdt_stats() {
                                stats.lines_added += crdt_stats.lines_added;
                                stats.lines_deleted += crdt_stats.lines_deleted;
                                stats.lines_modified += crdt_stats.lines_modified;
                                stats.tokens_added += crdt_stats.tokens_added;
                                stats.tokens_deleted += crdt_stats.tokens_deleted;
                                stats.tokens_replaced += crdt_stats.tokens_replaced;
                            }

                            // Track this as a deleted file
                            deleted_paths.push(path.clone());
                            recorded_paths.push(path.clone());
                            recorded_files.push(recorded);
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("{:?}", e)));
                            stats.errors += 1;
                        }
                    }
                }

                FileStatus::Modified | FileStatus::TypeChanged | FileStatus::PermissionsChanged => {
                    // Attribute-only changes still use the normal content path;
                    // an empty content diff is retained by pending_attributes.
                    modified_work.push((path.clone(), full_path.clone(), file_t0));
                }

                _ => {
                    skipped_paths.push(path.clone());
                    stats.files_skipped += 1;
                }
            }
        }

        // ── Phase 2: Process modified files in parallel ──────────────────
        //
        // Each modified file is diffed against its current view content (read
        // from the graph) and turned into BuiltHunks by `record_modified_file`.
        // Those hunks go through the normal globalize path in `assemble_change`,
        // which performs precise per-line vertex deletions/insertions. We do
        // NOT pre-globalize here: pre-globalization handled multi-line vertices
        // at whole-vertex granularity, which lost unchanged lines and produced
        // spurious conflicts on sequential edits.
        use rayon::prelude::*;

        enum ModifiedResult {
            // `RecordedFile` is large; box it so the enum's variants stay
            // similarly sized (clippy::large_enum_variant).
            Recorded(String, Box<RecordedFile>, Option<PreparedNameResolution>),
            Skipped(String, Option<PreparedNameResolution>),
            Error(String, String),
            Fatal(RecordError),
        }

        let par_results: Vec<ModifiedResult> = modified_work
            .par_iter()
            .map(|(path, _full_path, _)| {
                let working = match crate::content_filter::read_working_bytes(&self.root.join(path)) {
                    Ok(bytes) => bytes,
                    Err(error) => return ModifiedResult::Error(path.clone(), error.to_string()),
                };
                let filtered = match content_filter.clean(std::path::Path::new(path), &working) {
                    Ok(filtered) => filtered,
                    Err(error) => {
                        return ModifiedResult::Fatal(RecordError::Database(error.to_string()))
                    }
                };
                for warning in &filtered.warnings {
                    log::warn!("record: {warning}");
                }
                let repository_bytes = filtered.bytes;
                let opaque = content_filter.is_git_tracked(std::path::Path::new(path))
                    && (repository_bytes.len() as u64 > options.max_file_size()
                        || crate::content_filter::looks_binary(&repository_bytes));
                let preserve_mode = explicit_resolution_targets.contains(path);
                let resolution = match projected_name_conflicts.get(path) {
                    Some(conflict) => {
                        match prepare_name_resolution(
                            &shared_txn,
                            &shared_cached_txn,
                            &self.change_store,
                            &shared_graph_visibility,
                            path,
                            conflict,
                            &repository_bytes,
                            if preserve_mode {
                                NameResolutionPolicy::PreserveWorkingIdentity
                            } else {
                                NameResolutionPolicy::EditedContent
                            },
                        ) {
                            Ok(resolution) => Some(resolution),
                            Err(error) => {
                                return ModifiedResult::Error(path.clone(), error.to_string())
                            }
                        }
                    }
                    None => None,
                };
                let (file_inode, file_position) = match &resolution {
                    Some(resolution) => (resolution.winner.inode, resolution.winner.position),
                    None => match get_inode_position(&shared_txn, path) {
                        Ok(value) => value,
                        Err(error) => return ModifiedResult::Error(path.clone(), error),
                    },
                };

                // Old content = the file as the current view sees it in the graph.
                //
                // `had_fork_structure` is true when retrieval had to resolve a
                // fork or cyclic conflict (semantic merge / change-DAG
                // supersession) to produce this content — most concretely, a
                // fork left over from the orphan-view duplication bug.
                // That resolution isn't guaranteed to structurally match what a
                // plain, unambiguous checkout would render for the same graph
                // state, so a positional diff against it can corrupt the file.
                // We propagate the flag to `record_modified_file` via
                // `force_whole_file_replace` below instead of diffing against it.
                let (old_content, had_fork_structure) = {
                    use atomic_core::output::alive::RetrieveOptions;
                    let opts = RetrieveOptions::new()
                        .with_graph_visibility(shared_graph_visibility.clone());
                    match retrieve_content_with_filter_fast_with_fork_info(
                        &shared_cached_txn,
                        &self.change_store,
                        file_inode,
                        file_position,
                        opts,
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            return ModifiedResult::Error(path.clone(), format!("retrieve: {}", e))
                        }
                    }
                };

                // Resolve the file's existing **alive** CRDT branches in file
                // order. The CRDT op recipe binds Delete/Modify ops to these
                // real BranchIds so the old-content lines get tombstoned;
                // without them the CRDT walker leaves stale lines alive after a
                // modify/delete. Empty when the file has no CRDT rows yet.
                use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
                use atomic_core::crdt::tables::{decode_trunk_id, encode_branch_id};
                use atomic_core::pristine::CrdtTxnT;
                let inode_u64 = file_inode.get();
                let existing_trunk_id = match shared_txn.get_crdt_inode_trunk(inode_u64) {
                    Ok(Some(trunk_key)) => Some(decode_trunk_id(&trunk_key)),
                    _ => None,
                };
                let existing_branches = match existing_trunk_id {
                    Some(trunk_id) => {
                        match iter_trunk_branches_in_file_order(&shared_txn, trunk_id) {
                            Ok(all) => {
                                let mut alive = Vec::with_capacity(all.len());
                                for b in all {
                                    let bk = encode_branch_id(&b);
                                    if let Ok(Some(bd)) = shared_txn.get_crdt_branch(&bk) {
                                        if bd.state.is_alive() {
                                            alive.push(b);
                                        }
                                    }
                                }
                                alive
                            }
                            Err(_) => Vec::new(),
                        }
                    }
                    None => Vec::new(),
                };

                // Reuse the CRDT-materialized baseline only when every existing
                // branch belongs to a change visible on this view. That keeps
                // single-view linear histories on the CRDT cleanup path without
                // leaking sibling-view branches into cross-view recording.
                let can_use_crdt_old_content = !existing_branches.is_empty()
                    && existing_branches
                        .iter()
                        .all(|branch| shared_graph_visibility.contains(branch.change_id()));
                let crdt_old_content: Option<Vec<u8>> = if can_use_crdt_old_content {
                    match self.get_file_content_via_crdt(path.as_str()) {
                        Ok(Some(content)) if content == old_content => Some(content),
                        _ => None,
                    }
                } else {
                    None
                };
                let existing_branches_slice: Option<&[_]> = if existing_branches.is_empty() {
                    None
                } else {
                    Some(existing_branches.as_slice())
                };

                // Build line-level hunks + CRDT ops via the globalize-compatible
                // path. `record_modified_file` reads the new content from disk,
                // handles opaque/binary files, and emits BuiltHunks plus CRDT
                // ops; vertex resolution happens later during assembly.
                let mut detected =
                    atomic_core::record::workflow::DetectedFile::modified(path.as_str());
                detected.inode = Some(file_inode);
                detected.position = Some(file_position);
                let filtered_working_copy = Memory::new();
                filtered_working_copy.add_file(path, &repository_bytes);
                let file_options = if opaque {
                    log::warn!(
                        "record: Git-tracked '{}' is large or binary; preserving exact repository bytes as opaque content",
                        path
                    );
                    opaque_core_options.clone().force_whole_file_replace(true)
                } else if had_fork_structure {
                    if conflicted_paths.contains(path.as_str()) {
                        // The fork is a surfaced conflict this record is
                        // resolving — the normal resolve-over-markers flow.
                        // The whole-file-replace path is exactly right here;
                        // nothing is wrong, so don't alarm the user.
                        log::debug!(
                            "record: '{}' is resolving a surfaced conflict — \
                             recording the resolution as a whole-file replace \
                             (the conflict fork is superseded by this record)",
                            path
                        );
                    } else {
                        log::warn!(
                            "record: '{}' had fork/cyclic conflict structure in its graph — \
                             forcing a whole-file replace instead of a positional diff \
                             (whole-file-replace safety fallback; expected to be rare and self-healing)",
                            path
                        );
                    }
                    core_options.clone().force_whole_file_replace(true)
                } else {
                    core_options.clone()
                };
                match record_modified_file(
                    &filtered_working_copy,
                    &detected,
                    &old_content,
                    crdt_old_content.as_deref(),
                    &file_options,
                    existing_trunk_id,
                    existing_branches_slice,
                ) {
                    Ok(recorded) if recorded.is_empty() => {
                        ModifiedResult::Skipped(path.clone(), resolution)
                    }
                    Ok(mut recorded) => {
                        if opaque {
                            recorded.set_opaque_generated(true);
                        }
                        ModifiedResult::Recorded(path.clone(), Box::new(recorded), resolution)
                    }
                    Err(e) => ModifiedResult::Error(path.clone(), e),
                }
            })
            .collect();

        // Merge parallel results back into sequential state.
        let mut name_resolution_ops = Vec::new();
        let mut name_resolution_dependencies = std::collections::HashSet::new();
        let mut resolved_name_conflict_inodes = std::collections::HashSet::new();
        for result in par_results {
            let resolved_path = match &result {
                ModifiedResult::Recorded(path, _, _) | ModifiedResult::Skipped(path, _) => {
                    Some(path)
                }
                ModifiedResult::Error(path, msg) if name_conflicts.contains_key(path) => {
                    return Err(RecordError::Database(format!(
                        "cannot resolve recorded baseline for {path}: name conflict: {msg}"
                    )));
                }
                _ => None,
            };
            let mut resolved_name = false;
            if let Some(path) = resolved_path.filter(|path| {
                // --allow-conflict-markers records literal marker bytes; it
                // must not implicitly choose a winner for the name conflict.
                !options.get_allow_conflict_markers()
                    || std::fs::read(self.root.join(path))
                        .map(|bytes| {
                            super::materialize::first_conflict_marker_line(&bytes).is_none()
                        })
                        .unwrap_or(false)
            }) {
                if let Some(sides) = name_conflicts.get(path) {
                    use atomic_core::crdt::tables::decode_trunk_id;
                    use atomic_core::pristine::CrdtTxnT;
                    let (retained, retained_position) = selected_name_inodes[path];
                    if !sides.iter().any(|(inode, _)| *inode == retained) {
                        return Err(RecordError::Database(format!(
                            "cannot resolve recorded baseline for {path}: selected identity is not visible in the name conflict"
                        )));
                    }
                    for &(inode, position) in sides {
                        if inode == retained {
                            continue;
                        }
                        let bindings = super::materialize::live_names_for_path(
                            &shared_txn,
                            &self.change_store,
                            position,
                            path,
                            &shared_change_filter,
                        )?;
                        let mut unlinked = RecordedFile::new(path);
                        unlinked.set_inode(inode);
                        unlinked.set_position(position);
                        unlinked.set_name_conflict_resolution(retained_position, bindings);
                        // Namespace resolution preserves the semantic trunk,
                        // branches, and tokens. It is not a file deletion.
                        if let Some(key) = shared_txn
                            .get_crdt_inode_trunk(inode.get())
                            .map_err(|e| RecordError::Database(e.to_string()))?
                        {
                            let trunk = decode_trunk_id(&key);
                            unlinked.set_crdt_ops(atomic_core::change::FileOps::new(
                                trunk,
                                path.clone(),
                                None,
                            ));
                        }
                        stats.hunks_created += 2; // identity selection + namespace unlink
                        stats.edges_modified += 1;
                        recorded_files.push(unlinked);
                    }
                    resolved_name = true;
                }
            }
            match result {
                ModifiedResult::Recorded(path, recorded, resolution) => {
                    if let Some(resolution) = resolution {
                        name_resolution_ops.push(resolution.operation);
                        name_resolution_dependencies.extend(resolution.dependencies);
                        resolved_name_conflict_inodes.extend(resolution.conflict_inodes);
                    }
                    stats.files_recorded += 1;
                    stats.hunks_created += recorded.hunk_count();
                    // Account for the graph effects of a modified file so the
                    // record summary and stats-based callers see real numbers.
                    // Each Insert/Replace hunk introduces a new content span;
                    // each Delete/Replace marks existing content deleted.
                    for hunk in recorded.hunks() {
                        use atomic_core::record::workflow::graph_op::BuiltHunkKind;
                        match hunk.kind {
                            BuiltHunkKind::Insert => stats.vertices_added += 1,
                            BuiltHunkKind::Replace => {
                                stats.vertices_added += 1;
                                stats.edges_modified += 1;
                            }
                            BuiltHunkKind::Delete => stats.edges_modified += 1,
                        }
                        stats.content_bytes += hunk.content_len();
                    }
                    recorded_paths.push(path);
                    recorded_files.push(*recorded);
                }
                ModifiedResult::Skipped(path, resolution) => {
                    if let Some(resolution) = resolution {
                        name_resolution_ops.push(resolution.operation);
                        name_resolution_dependencies.extend(resolution.dependencies);
                        resolved_name_conflict_inodes.extend(resolution.conflict_inodes);
                        recorded_paths.push(path);
                        stats.hunks_created += 1;
                    } else if resolved_name {
                        // The identity was selected in this loop's Recorded
                        // sibling: a marker-stripped file with no other edits
                        // is the resolution itself, not a skip (dev #203).
                        recorded_paths.push(path);
                        stats.files_recorded += 1;
                    } else {
                        skipped_paths.push(path);
                        stats.files_skipped += 1;
                    }
                }
                ModifiedResult::Error(path, msg) => {
                    errors.push((path, msg));
                    stats.errors += 1;
                }
                ModifiedResult::Fatal(error) => return Err(error),
            }
        }

        if trace_record {
            eprintln!(
                "[record] file loop total: {:.1}ms ({} files, {} modified)",
                record_t0.elapsed().as_secs_f64() * 1000.0,
                files_to_record.len(),
                modified_work.len(),
            );
        }

        // Check if we actually recorded anything
        if recorded_files.is_empty()
            && name_resolution_ops.is_empty()
            && pending_attributes.is_empty()
        {
            // Fail closed with the REAL cause: per-file errors (e.g. a
            // refused name-conflict resolution) must not be swallowed into
            // a misleading "working copy is clean" — the caller could
            // believe nothing was pending when a resolution actually
            // refused. (Review ::15 resolution support, 2026-09-16.)
            if let Some((path, message)) = errors.first() {
                return Err(RecordError::Database(format!(
                    "record failed for {} path(s); first error at '{path}': {message}",
                    errors.len()
                )));
            }
            // A recordable path that produced NO outcome and NO error means
            // the file loop silently dropped it — fail closed with the
            // recorded-path diagnostics instead of a misleading clean-copy.
            if !recorded_paths.is_empty() || !skipped_paths.is_empty() {
                return Err(RecordError::Database(format!(
                    "record produced no change but tracked paths remain: \
                     recorded={recorded_paths:?} skipped={skipped_paths:?}"
                )));
            }
            if let Some(cleanup) = conflict_cleanup.take() {
                return Ok(conflict_cleanup_outcome(final_header.clone(), cleanup));
            }
            return Err(RecordError::NothingToRecord);
        }

        // Assemble the change from the recorded files.
        //
        // We wrap the transaction in a ViewGraph so that the entire
        // globalization pipeline (retrieve_graph, iter_adjacent, etc.)
        // transparently sees only edges introduced by changes visible
        // on the current view.  No manual filter threading needed.
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RecordError::Database(e.to_string()))?;

        // Wrap the read transaction in CachedGraphTxn to avoid reopening
        // the GRAPH table on every find_block/iter_adjacent call during
        // globalization. This alone eliminates ~90% of the per-vertex
        // overhead in the recording pipeline.
        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RecordError::Database(e.to_string()))?;

        let view_graph = ViewGraph::new(&cached_txn, shared_graph_visibility.clone());

        let assembly_options = options.to_assembly_options();

        let assemble_t0 = std::time::Instant::now();
        let mut change = if recorded_files.is_empty() {
            Change::empty(final_header)
        } else {
            assemble_change(
                &view_graph,
                &recorded_files,
                final_header,
                &assembly_options,
            )?
            .into_change()
        };
        let created_attribute_targets: std::collections::HashMap<String, Position<Option<Hash>>> =
            change
                .hunks()
                .iter()
                .filter_map(|operation| match operation {
                    GraphOp::FileAdd {
                        add_inode, path, ..
                    } => Some((
                        path.clone(),
                        Position {
                            change: None,
                            pos: add_inode.start,
                        },
                    )),
                    _ => None,
                })
                .collect();
        let semantic_trunks: std::collections::HashMap<String, atomic_core::crdt::TrunkId> = change
            .file_ops()
            .iter()
            .map(|ops| (ops.path().to_string(), ops.trunk_id()))
            .collect();
        for pending in pending_attributes {
            let target = match pending.position {
                Some(position) => {
                    let hash = shared_txn
                        .get_external(position.change)
                        .map_err(|error| RecordError::Database(error.to_string()))?
                        .ok_or_else(|| {
                            RecordError::Database(format!(
                                "attribute target change {:?} has no external hash",
                                position.change
                            ))
                        })?;
                    change.hashed.dependencies.push(hash);
                    Position {
                        change: Some(hash),
                        pos: position.pos,
                    }
                }
                None => *created_attribute_targets
                    .get(&pending.path)
                    .ok_or_else(|| {
                        RecordError::Database(format!(
                            "new attribute path '{}' has no FileAdd inode",
                            pending.path
                        ))
                    })?,
            };
            change.hashed.dependencies.extend(pending.dependencies);
            change.add_hunk(GraphOp::SetAttr {
                inode: target,
                path: pending.path.clone(),
                value: pending.value,
            });

            let trunk = pending
                .trunk
                .or_else(|| semantic_trunks.get(&pending.path).copied())
                .ok_or_else(|| {
                    RecordError::Database(format!(
                        "attribute path '{}' has no semantic trunk",
                        pending.path
                    ))
                })?;
            let semantic = match pending.value {
                atomic_core::change::InodeAttr::Mode(mode) => {
                    atomic_core::change::FileOps::set_mode(trunk, pending.path, mode)
                        .map_err(|error| RecordError::Database(error.to_string()))?
                }
                atomic_core::change::InodeAttr::Kind(kind) => {
                    atomic_core::change::FileOps::set_kind(trunk, pending.path, kind)
                }
            };
            change.add_file_ops(semantic);
        }
        for operation in name_resolution_ops {
            change.add_hunk(operation);
        }

        change
            .hashed
            .dependencies
            .extend(name_resolution_dependencies);
        change.hashed.dependencies.sort();
        change.hashed.dependencies.dedup();
        if !move_evidence.is_empty() {
            crate::record::merge_move_evidence(&mut change, &move_evidence)
                .map_err(|error| RecordError::ChangeStore(error.to_string()))?;
        }
        // Application metadata (e.g. the CB-7B adoption attribution marker)
        // is part of the hashed change content: it is set before
        // serialization so the marker is content-addressed with the change
        // and can never be edited after the fact.
        let application_metadata = options.get_metadata_bytes();
        if !application_metadata.is_empty() {
            change.hashed.metadata = application_metadata.to_vec();
        }
        lifecycle.verify_promotion_content(&change, self)?;
        change = lifecycle.classify(change)?;
        if trace_record {
            eprintln!(
                "[record] assemble_change: {:.1}ms",
                assemble_t0.elapsed().as_secs_f64() * 1000.0,
            );
        }

        stats.dependency_count = change.dependencies().len();

        // Unhashed data (the agent's condensed transcript, reasoning) rides
        // the change from here: the local change store's file and — the
        // point of the seam — a remote sandbox's SubmitChange bytes, which
        // carry exactly what this record serialized. It used to be attached
        // AFTER the record and re-saved to a store a sandbox never reads,
        // so expeditions recorded changes whose provenance said nothing
        // about how they were made.
        if let Some(unhashed) = options.get_unhashed() {
            let entry = change.unhashed.get_or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = entry.as_object_mut() {
                for (key, value) in unhashed.as_object().into_iter().flatten() {
                    obj.insert(key.clone(), value.clone());
                }
            }
        }

        let serialize_t0 = std::time::Instant::now();
        // Serialize to V3 format and compute content hash.
        // We keep the raw V3 bytes so we can save them directly to disk
        // without re-serializing (which would produce a different hash).
        //
        // Signing: the SIGNATURE section is unhashed, so the content hash is
        // a pure function of the unsigned content. We therefore serialize
        // unsigned first, obtain the content hash, sign it, then re-serialize
        // with the signature attached — the hash is identical both times and
        // the change's identity is unaffected by who signed it.
        let mut v3_bytes = Vec::new();
        let computed_hash = change
            .serialize(&mut v3_bytes)
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;

        let change = if let Some(signing) = options.signing_identity() {
            let signature = atomic_core::change::signing::sign_change(
                &signing.signer_did,
                &signing.secret_key,
                &computed_hash,
                chrono::Utc::now().timestamp(),
            );
            let mut signed = change;
            signed.signature = Some(signature);
            let mut signed_bytes = Vec::new();
            let signed_hash = signed
                .serialize(&mut signed_bytes)
                .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
            debug_assert_eq!(
                signed_hash, computed_hash,
                "SIGNATURE section must not affect the change hash"
            );
            // Round-trip the signed bytes for a clean deserialized form
            let (final_signed, verified_hash) =
                Change::deserialize(&mut signed_bytes.as_slice())
                    .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
            debug_assert_eq!(signed_hash, verified_hash);
            v3_bytes = signed_bytes;
            final_signed
        } else {
            // Unsigned path: round-trip the unsigned bytes as before
            let (final_change, verified_hash) = Change::deserialize(&mut v3_bytes.as_slice())
                .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
            debug_assert_eq!(computed_hash, verified_hash);
            final_change
        };

        if trace_record {
            eprintln!(
                "[record] serialize + deserialize: {:.1}ms",
                serialize_t0.elapsed().as_secs_f64() * 1000.0,
            );
        }

        let mut outcome = RecordOutcome::new(change, computed_hash, stats);
        // Stash the original V3 bytes so save_change can write them directly
        // instead of re-serializing (which may produce a different hash).
        outcome.set_v3_bytes(v3_bytes);

        // Add recorded/skipped/deleted files to outcome
        for path in recorded_paths {
            outcome.add_recorded_file(path);
        }
        for path in deleted_paths {
            outcome.add_deleted_file(path);
        }
        for path in skipped_paths {
            outcome.add_skipped_file(path);
        }
        for (path, error) in errors {
            outcome.add_error(path, error);
        }

        let operation_context = if options.get_save_to_store() && options.get_apply_after_record() {
            let operation_lock = self
                .try_lock_operation(working_copy)
                .map_err(RecordError::Repository)?;
            if let OperationHeadState::Diverged(heads) = self
                .consolidate_operation_heads_locked(&operation_lock)
                .map_err(RecordError::Repository)?
            {
                return Err(RecordError::Repository(
                    RepositoryError::OperationHeadsDiverged {
                        scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                            .to_string(),
                        heads: heads.iter().map(ToString::to_string).collect(),
                    },
                ));
            }
            let (before_state, after_state, transitions, operation_changes) = {
                let txn = self.pristine.read_txn().map_err(|error| {
                    RecordError::Repository(RepositoryError::Database(error.to_string()))
                })?;
                let view = txn
                    .get_view(&target_view)
                    .map_err(|error| {
                        RecordError::Repository(RepositoryError::Database(error.to_string()))
                    })?
                    .ok_or_else(|| {
                        RecordError::Repository(RepositoryError::ViewNotFound {
                            name: target_view.clone(),
                        })
                    })?;
                let before_record = txn
                    .get_working_copy(working_copy)
                    .map_err(|error| {
                        RecordError::Repository(RepositoryError::Database(error.to_string()))
                    })?
                    .ok_or_else(|| {
                        RecordError::Repository(RepositoryError::WorkingCopyRecordNotFound {
                            id: working_copy,
                        })
                    })?;

                let removal = lifecycle.removal();
                let sequence = if removal
                    .as_ref()
                    .is_some_and(|(view_name, _)| *view_name == target_view)
                {
                    view.change_count.saturating_sub(1)
                } else {
                    view.change_count
                };
                let mut after_view_state = Merkle::ZERO;
                for row in txn.iter_changes(&view, 0).map_err(|error| {
                    RecordError::Repository(RepositoryError::Database(error.to_string()))
                })? {
                    let (_, node_id, _) = row.map_err(|error| {
                        RecordError::Repository(RepositoryError::Database(error.to_string()))
                    })?;
                    let hash = txn
                        .get_external(node_id)
                        .map_err(|error| {
                            RecordError::Repository(RepositoryError::Database(error.to_string()))
                        })?
                        .ok_or_else(|| {
                            RecordError::Repository(RepositoryError::ChangeNotFound {
                                hash: node_id.to_string(),
                            })
                        })?;
                    if removal.as_ref().is_some_and(|(view_name, removed)| {
                        *view_name == target_view && *removed == hash
                    }) {
                        continue;
                    }
                    after_view_state = after_view_state.next(&hash);
                }
                after_view_state = after_view_state.next(&computed_hash);

                let mut after_record = before_record.clone();
                if target_view == desired_view {
                    after_record.desired_state = after_view_state;
                }
                let mut transitions = Vec::new();
                let mut operation_changes = vec![computed_hash];
                if let Some((removal_view_name, removed)) = removal {
                    let removal_view = txn
                        .get_view(removal_view_name)
                        .map_err(|error| {
                            RecordError::Repository(RepositoryError::Database(error.to_string()))
                        })?
                        .ok_or_else(|| {
                            RecordError::Repository(RepositoryError::ViewNotFound {
                                name: removal_view_name.to_string(),
                            })
                        })?;
                    let removed_id = txn
                        .get_internal(&removed)
                        .map_err(|error| {
                            RecordError::Repository(RepositoryError::Database(error.to_string()))
                        })?
                        .ok_or_else(|| {
                            RecordError::Repository(RepositoryError::ChangeNotFound {
                                hash: removed.to_base32(),
                            })
                        })?;
                    let removed_sequence = txn
                        .get_change_seq(&removal_view, removed_id)
                        .map_err(|error| {
                            RecordError::Repository(RepositoryError::Database(error.to_string()))
                        })?
                        .ok_or_else(|| {
                            RecordError::Repository(RepositoryError::ChangeNotInView {
                                hash: removed.to_base32(),
                                view: removal_view_name.to_string(),
                            })
                        })?;
                    transitions.push(MetadataTransition {
                        target: MetadataTarget::ViewChange {
                            view: removal_view_name.to_string(),
                            change: removed,
                        },
                        expected_old: MetadataValue::Sequence(removed_sequence),
                        expected_new: MetadataValue::Absent,
                    });
                    operation_changes.push(removed);
                }
                transitions.push(MetadataTransition {
                    target: MetadataTarget::ViewChange {
                        view: target_view.clone(),
                        change: computed_hash,
                    },
                    expected_old: MetadataValue::Absent,
                    expected_new: MetadataValue::Sequence(sequence),
                });
                (
                    RepoStateRef {
                        view: Some(ViewStateRef {
                            name: target_view.clone(),
                            state: view.state,
                            set_id: None,
                        }),
                        working_copy: Some(super::operation::working_copy_state_ref(before_record)),
                        git: None,
                    },
                    RepoStateRef {
                        view: Some(ViewStateRef {
                            name: target_view.clone(),
                            state: after_view_state,
                            set_id: None,
                        }),
                        working_copy: Some(super::operation::working_copy_state_ref(after_record)),
                        git: None,
                    },
                    transitions,
                    operation_changes,
                )
            };
            let operation = self
                .prepare_metadata_operation(
                    &operation_lock,
                    OperationKind::Record,
                    None,
                    before_state,
                    after_state,
                    transitions,
                    operation_changes,
                    ActorRef::System {
                        name: "repository-record".to_string(),
                    },
                    super::operation::current_operation_timestamp_ms(),
                )
                .map_err(RecordError::Repository)?;
            // R3 (CB-7B): refuse a dependency-invalid snapshot BEFORE it is
            // published (RFC §12.3). The prepared membership operation is
            // aborted so the journal shows an immutable refusal, not an
            // applied-then-compensated publication: no snapshot change that
            // depends on a snapshot-kind change is ever saved or applied.
            if lifecycle.is_snapshot_publication()
                && outcome.change().dependencies().iter().any(|dependency| {
                    self.load_change(dependency)
                        .map(|change| change.kind().is_snapshot())
                        .unwrap_or(false)
                })
            {
                self.abort_prepared_metadata_operation(&operation_lock, &operation)
                    .map_err(RecordError::Repository)?;
                return Err(RecordError::Repository(RepositoryError::InvalidOperation {
                    message: format!(
                        "snapshot change {} depends on a snapshot change; refusing to \
                         publish a dependency-invalid snapshot",
                        computed_hash.to_base32()
                    ),
                }));
            }
            Some((operation_lock, operation))
        } else {
            None
        };

        // Save to store if requested.
        // Use the original V3 bytes (not re-serialized) to ensure the hash
        // on disk matches the hash registered in the pristine graph.
        if options.get_save_to_store() {
            let save_start = std::time::Instant::now();
            if let Some(v3_bytes) = outcome.v3_bytes() {
                // Fast path: write the exact V3 bytes that produced computed_hash
                self.save_change_bytes(&computed_hash, v3_bytes, outcome.change())
                    .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
            } else {
                // Fallback: re-serialize (may produce different hash — legacy path)
                self.save_change(outcome.change())
                    .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
            }
            outcome.set_saved(true);
            if trace_record {
                eprintln!(
                    "[record] save_change complete elapsed={:?}",
                    save_start.elapsed()
                );
            }
        }

        // Apply if requested
        // We use write_recorded() instead of insert_change() because it creates
        // the TREE and INODES entries for FileAdd hunks, which is necessary
        // for the file to be recognized as tracked with graph content.
        let mut apply_error = None;
        if options.get_apply_after_record() && outcome.was_saved() {
            let apply_opts = InsertOptions::default().view(&target_view);
            let apply_t0 = std::time::Instant::now();
            #[cfg(test)]
            let inject_apply_failure = RECORD_APPLY_FAULT.with(|fault| fault.replace(false));
            #[cfg(not(test))]
            let inject_apply_failure = false;
            let apply_result = if inject_apply_failure {
                Err(RepositoryError::InvalidOperation {
                    message: "injected record apply failure".into(),
                })
            } else {
                self.write_recorded_replacing(&outcome, apply_opts, lifecycle.removal())
            };
            match apply_result {
                Ok(apply_outcome) => {
                    outcome.set_applied(apply_outcome.new_state);
                    if target_view == desired_view {
                        self.refresh_working_copy_desired_state(working_copy)
                            .map_err(RecordError::Repository)?;
                    }

                    // Update file index for all recorded/added files.
                    // This snapshots the filesystem metadata + content hash AFTER
                    // the record, so subsequent status() calls can skip unchanged
                    // files (mtime+size match) or avoid graph reconstruction
                    // (compare stored content hash instead).
                    if target_view == desired_view {
                        if let Ok(mut idx_txn) = self.pristine.write_txn() {
                            let file_index_start = std::time::Instant::now();
                            for path_str in outcome.recorded_files() {
                                // Strip directory markers like "dir/ (directory)"
                                let clean_path =
                                    path_str.strip_suffix("/ (directory)").unwrap_or(path_str);
                                let abs_path = self.root.join(clean_path);
                                if let Ok(metadata) = std::fs::metadata(&abs_path) {
                                    use std::time::SystemTime;
                                    let mtime =
                                        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                                    let duration = mtime
                                        .duration_since(SystemTime::UNIX_EPOCH)
                                        .unwrap_or_default();
                                    let content_hash = std::fs::read(&abs_path)
                                        .map(|bytes| Hash::of(&bytes))
                                        .unwrap_or(Hash::ZERO);
                                    let _ = idx_txn.put_working_copy_file_index(
                                        working_copy,
                                        clean_path,
                                        duration.as_secs() as i64,
                                        duration.subsec_nanos(),
                                        metadata.len(),
                                        &content_hash,
                                    );
                                }
                            }

                            // Also update FILE_INDEX for skipped files.
                            //
                            // Files are skipped when their graph content already
                            // matches the working copy (old == new). Without this,
                            // files missing a FILE_INDEX entry are perpetually
                            // reported as Modified by status (conservative mtime
                            // check) and perpetually skipped by record (content
                            // unchanged) — an infinite loop.
                            for path_str in outcome.skipped_files() {
                                let clean_path =
                                    path_str.strip_suffix("/ (directory)").unwrap_or(path_str);
                                let abs_path = self.root.join(clean_path);
                                if let Ok(metadata) = std::fs::metadata(&abs_path) {
                                    use std::time::SystemTime;
                                    let mtime =
                                        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                                    let duration = mtime
                                        .duration_since(SystemTime::UNIX_EPOCH)
                                        .unwrap_or_default();
                                    let content_hash = std::fs::read(&abs_path)
                                        .map(|bytes| Hash::of(&bytes))
                                        .unwrap_or(Hash::ZERO);
                                    let _ = idx_txn.put_working_copy_file_index(
                                        working_copy,
                                        clean_path,
                                        duration.as_secs() as i64,
                                        duration.subsec_nanos(),
                                        metadata.len(),
                                        &content_hash,
                                    );
                                }
                            }

                            // Remove FILE_INDEX entries for deleted and moved-from
                            // paths so stale source metadata cannot survive a rename.
                            for path_str in outcome.deleted_files() {
                                let _ = idx_txn.del_working_copy_file_index(working_copy, path_str);
                            }
                            for path_str in &move_source_paths {
                                let _ = idx_txn.del_working_copy_file_index(working_copy, path_str);
                            }

                            // Clear persisted conflict state for recorded files:
                            // recording without markers IS the resolution.
                            let record_view = target_view.clone();
                            if let Ok(Some(view)) = idx_txn.get_view(&record_view) {
                                for path_str in outcome.recorded_files() {
                                    let clean_path =
                                        path_str.strip_suffix("/ (directory)").unwrap_or(path_str);
                                    if let Ok(Some(inode)) = idx_txn.get_inode(clean_path) {
                                        let _ = idx_txn.del_conflicts(view.id, inode.get());
                                    }
                                }
                                for inode in &resolved_name_conflict_inodes {
                                    let _ = idx_txn.del_conflicts(view.id, inode.get());
                                }
                            }

                            let _ = idx_txn.commit();
                            if trace_record {
                                eprintln!(
                                    "[record] file_index_update complete elapsed={:?}",
                                    file_index_start.elapsed()
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    apply_error = Some(e);
                }
            }
            if trace_record {
                eprintln!(
                    "[record] write_recorded (apply): {:.1}ms",
                    apply_t0.elapsed().as_secs_f64() * 1000.0,
                );
            }
        }

        if let Some((operation_lock, operation)) = operation_context {
            if outcome.was_applied() {
                self.finalize_operation_verified(&operation_lock, operation.id())
                    .map_err(RecordError::Repository)?;
            } else {
                self.abort_prepared_metadata_operation(&operation_lock, &operation)
                    .map_err(RecordError::Repository)?;
            }
        }

        // Saved change bytes are not a successful record until the change
        // lands in the target view. Abort the prepared operation above before
        // returning the original error; all callers (including libatomic)
        // then stop instead of publishing a stale view as a successful record.
        if let Some(error) = apply_error {
            return Err(RecordError::Repository(error));
        }

        // Deflate vault working copy changes (if vault is initialized)
        if options.get_sync_vault() && self.has_vault().unwrap_or(false) {
            match self.vault_record_working_copy() {
                Ok(vault_paths) if !vault_paths.is_empty() => {
                    outcome.set_vault_paths(vault_paths);
                }
                Ok(_) => {} // No vault changes
                Err(e) => {
                    // Log the error but don't fail the record — vault sync
                    // is best-effort during record
                    log::warn!("Failed to sync vault working copy: {}", e);
                }
            }
        }

        // Auto-update the enrich database (KG + content search index) with the
        // new change (best-effort). The content index step is a no-op unless an
        // index already exists, so this maintains — never builds — it.
        if options.get_enrich_kg() && outcome.was_saved() {
            let hash = *outcome.hash();
            if let Err(e) = self.kg_enrich_change(working_copy, &hash) {
                log::debug!("KG enrich for change: {}", e);
            }
            if let Err(e) = crate::content_search::update_content_index_paths(
                self.root(),
                outcome.recorded_files(),
            ) {
                log::debug!("content index update for change: {}", e);
            }
        }

        if trace_record {
            eprintln!(
                "[record] TOTAL: {:.1}ms",
                record_t0.elapsed().as_secs_f64() * 1000.0,
            );
        }

        Ok(outcome)
    }

    /// Record changes with a simple message.
    ///
    /// This is a convenience method that creates a change header with just
    /// a message.
    ///
    /// # Arguments
    ///
    /// * `working_copy` - The validated physical working-copy identity
    /// * `message` - The change message
    /// * `options` - Recording options
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.record_with_message(working_copy, "Fix bug", RecordOptions::default())?;
    /// ```
    pub fn record_with_message(
        &self,
        working_copy: WorkingCopyId,
        message: impl Into<String>,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        self.validate_working_copy(working_copy)
            .map_err(RecordError::Repository)?;
        let header = ChangeHeader::builder().message(message).build();
        self.record(working_copy, header, options)
    }

    /// Fast path for external importers (e.g. git-import) that already have
    /// pre-built `RecordedFile`s and just need globalization + assembly + hashing.
    ///
    /// Returns `(Change, Hash)`.
    pub fn assemble_and_hash(
        &self,
        view_name: &str,
        header: ChangeHeader,
        recorded_files: &[atomic_core::record::workflow::RecordedFile],
    ) -> Result<(Change, Hash), RecordError> {
        use atomic_core::record::workflow::assemble_change;
        use atomic_core::record::workflow::assembly::AssemblyOptions;

        let overall_start = std::time::Instant::now();

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RecordError::Database(e.to_string()))?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| RecordError::Database(e.to_string()))?
            .ok_or_else(|| {
                RecordError::Repository(RepositoryError::ViewNotFound {
                    name: view_name.to_string(),
                })
            })?;
        let visibility = graph_visibility_closure(&txn, &view).map_err(RecordError::Repository)?;
        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RecordError::Database(e.to_string()))?;
        let view_graph = ViewGraph::new(&cached_txn, visibility);
        let assembly_options = AssemblyOptions::default();

        let assembly_result =
            assemble_change(&view_graph, recorded_files, header, &assembly_options)?;

        let change = assembly_result.into_change();
        log::debug!(
            "assemble_and_hash: assembly complete, content_size={} hunks={}",
            change.contents.len(),
            change.hunks().len(),
        );

        let step = std::time::Instant::now();
        let mut v3_bytes = Vec::new();
        let hash = change
            .serialize(&mut v3_bytes)
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
        let serialize_ms = step.elapsed().as_millis();

        let step = std::time::Instant::now();
        let (final_change, _) = Change::deserialize(&mut v3_bytes.as_slice())
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
        let deserialize_ms = step.elapsed().as_millis();

        let total_ms = overall_start.elapsed().as_millis();
        if serialize_ms + deserialize_ms > 100 {
            log::warn!(
                "assemble_and_hash: serialize={}ms ({} bytes) deserialize={}ms total={}ms",
                serialize_ms,
                v3_bytes.len(),
                deserialize_ms,
                total_ms,
            );
        } else {
            log::debug!(
                "assemble_and_hash: serialize={}ms ({} bytes) deserialize={}ms total={}ms",
                serialize_ms,
                v3_bytes.len(),
                deserialize_ms,
                total_ms,
            );
        }

        Ok((final_change, hash))
    }

    /// Look up a file's inode and graph position from the pristine.
    ///
    /// Returns `None` if the file is not tracked or has no graph position.
    pub fn get_inode_and_position(
        &self,
        path: &str,
    ) -> Result<Option<(Inode, Position<NodeId>)>, RepositoryError> {
        use atomic_core::pristine::TreeTxnT;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let inode = match txn
            .get_inode(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(i) => i,
            None => return Ok(None),
        };

        let position = match txn
            .inode_position(inode)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(p) => p,
            None => return Ok(None),
        };

        Ok(Some((inode, position)))
    }

    /// Resolve a recorded path's identity through the canonical graph and an
    /// explicit assembly closure. TREE alone cannot decide whether the path
    /// exists or which inode owns it. Genuine name conflicts remain errors.
    pub fn get_inode_and_position_on_view_excluding(
        &self,
        path: &str,
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<Option<(Inode, Position<NodeId>)>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visibility = super::filter::assembly_visibility_excluding(&txn, &view, excluded)?;
        let projection = self.project_tree_for_visibility_scoped(&txn, &visibility, Some(path))?;
        if projection.name_conflicts.contains_key(path) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path '{path}' has an unresolved name conflict in view '{view_name}'"
                ),
            });
        }
        Ok(projection
            .present
            .get(path)
            .map(|item| (item.inode, item.position)))
    }

    /// Record all changes with a message.
    ///
    /// This is a convenience method that records all modified files.
    ///
    /// # Arguments
    ///
    /// * `working_copy` - The validated physical working-copy identity
    /// * `message` - The change message
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.record_all(working_copy, "Update all files")?;
    /// ```
    pub fn record_all(
        &self,
        working_copy: WorkingCopyId,
        message: impl Into<String>,
    ) -> Result<RecordOutcome, RecordError> {
        self.validate_working_copy(working_copy)
            .map_err(RecordError::Repository)?;
        let options = RecordOptions::new().with_all(true);
        self.record_with_message(working_copy, message, options)
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_file_moves(
    _working_copy: WorkingCopyId,
    txn: &atomic_core::pristine::ReadTxn,
    cached: &CachedGraphTxn<'_>,
    store: &ChangeStore,
    visibility: &GraphVisibilityClosure,
    entries: &[FileStatusEntry],
    present: &std::collections::HashMap<String, super::name_resolution::ProjectedPathClaim>,
    absent: &std::collections::HashMap<String, super::deferred_tree::ProjectedAbsent>,
    root: &Path,
    content_filter: &dyn crate::content_filter::ContentFilter,
    detect_heuristic_moves: bool,
) -> Result<FileMovePlanning, RecordError> {
    use crate::record::{
        AuthoritativeMove, LossNote, MoveAuthority, MoveBasis, ProbableMove, RenameCandidate,
        PROBABLE_MOVE_THRESHOLD_BPS,
    };

    #[derive(Debug, Clone)]
    struct Candidate {
        old_path: String,
        new_path: String,
        inode: Inode,
        score: u16,
        basis: MoveBasis,
    }

    let load_old_content =
        |inode: Inode, position: Position<NodeId>| -> Result<Vec<u8>, RecordError> {
            super::content::retrieve_content_with_filter_fast_with_fork_info(
                cached,
                store,
                inode,
                position,
                atomic_core::output::alive::RetrieveOptions::new()
                    .with_graph_visibility(visibility.clone()),
            )
            .map(|(content, _)| content)
            .map_err(|error| RecordError::Database(error.to_string()))
        };
    let make_plan = |old_path: &str,
                     new_path: &str,
                     inode: Inode|
     -> Result<FileMovePlan, RecordError> {
        let source = present.get(old_path).ok_or_else(|| {
            RecordError::Database(format!(
                "FileMove source '{}' is not one uncontested visible path claim",
                old_path
            ))
        })?;
        let inode_position = txn
            .inode_position(inode)
            .map_err(|error| RecordError::Database(error.to_string()))?
            .ok_or_else(|| {
                RecordError::Database(format!(
                    "FileMove inode {} has no graph position",
                    inode.get()
                ))
            })?;
        if source.inode != inode || source.position != inode_position {
            return Err(RecordError::Database(format!(
                "FileMove source '{}' does not match stable inode {}",
                old_path,
                inode.get()
            )));
        }
        if source.claims.len() != 1 {
            return Err(RecordError::Database(format!(
                "FileMove source '{}' has {} causally maximal structural claims; refusing ambiguity",
                old_path,
                source.claims.len()
            )));
        }
        Ok(FileMovePlan {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
            inode,
            claimant: source.position,
            source: source.claims[0],
            old_content: load_old_content(inode, source.position)?,
        })
    };

    let entries_by_path: std::collections::BTreeMap<String, &FileStatusEntry> = entries
        .iter()
        .map(|entry| (entry.path().to_string_lossy().to_string(), entry))
        .collect();
    let mut planning = FileMovePlanning::default();
    let mut used_old = std::collections::BTreeSet::new();
    let mut used_new = std::collections::BTreeSet::new();

    // `atomic mv` stages the original inode at its destination in TREE. Compare
    // that explicit stable-inode relationship with graph-backed visible claims;
    // content is deliberately irrelevant, so a complete rewrite still moves.
    let is_file_or_symlink = |path: &Path| {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() || metadata.file_type().is_symlink())
    };
    let mut visible_paths: Vec<_> = present.keys().cloned().collect();
    visible_paths.sort();
    for old_path in visible_paths {
        let source = &present[&old_path];
        let current_path = txn
            .get_path(source.inode)
            .map_err(|error| RecordError::Database(error.to_string()))?;
        let Some(new_path) = current_path else {
            continue;
        };
        if new_path == old_path
            || present.contains_key(&new_path)
            || is_file_or_symlink(&root.join(&old_path))
            || !is_file_or_symlink(&root.join(&new_path))
            // Attribute changes take precedence over Added in status. They
            // must not hide an explicitly staged stable-inode move.
            || !entries_by_path.get(&new_path).is_some_and(|entry| {
                matches!(
                    entry.status(),
                    FileStatus::Added | FileStatus::PermissionsChanged | FileStatus::TypeChanged
                )
            })
        {
            continue;
        }
        planning
            .moves
            .push(make_plan(&old_path, &new_path, source.inode)?);
        planning
            .evidence
            .insert_authoritative(AuthoritativeMove::new(
                &old_path,
                &new_path,
                source.inode,
                MoveAuthority::StableInodeProjection,
            ));
        used_old.insert(old_path);
        used_new.insert(new_path);
    }

    let mut deleted: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry.status() == FileStatus::Deleted && entry.details() != Some("directory")
        })
        .collect();
    deleted.sort_by(|left, right| left.path().cmp(right.path()));

    // Historical same-inode paths are candidates, not authority. Even when
    // heuristic detection is disabled, a simultaneous deletion and recreation
    // at such a path must be a fresh add rather than an implicit undelete.
    let deleted_inodes: std::collections::BTreeSet<Inode> = deleted
        .iter()
        .filter_map(|entry| present.get(entry.path().to_string_lossy().as_ref()))
        .map(|source| source.inode)
        .collect();
    let historical_destinations: std::collections::BTreeSet<String> = entries
        .iter()
        .filter(|entry| entry.status() == FileStatus::Added && entry.details() != Some("directory"))
        .filter_map(|entry| {
            let path = entry.path().to_string_lossy().to_string();
            absent
                .get(&path)
                .filter(|projected| deleted_inodes.contains(&projected.inode))
                .map(|_| path)
        })
        .collect();

    if !detect_heuristic_moves {
        planning
            .fresh_additions
            .extend(historical_destinations.difference(&used_new).cloned());
        planning
            .moves
            .sort_by(|left, right| left.old_path.cmp(&right.old_path));
        return Ok(planning);
    }

    // With detection enabled, only content evidence can justify a
    // `ProbableMove`. This prevents unrelated bytes recreated at an old path
    // from inheriting the current inode.
    let mut destinations: Vec<(String, Vec<u8>, Option<Inode>)> = Vec::new();
    for entry in entries {
        let path = entry.path().to_string_lossy().to_string();
        let expected_inode = match entry.status() {
            FileStatus::Untracked => None,
            // AddFiles may stage a destination before Record (including the
            // RPC record -a flow). A staged-only inode is not recorded file
            // identity; allow the same unique content matching as Untracked.
            FileStatus::Added if !present.contains_key(&path) && !absent.contains_key(&path) => {
                let Some(inode) = entry.inode() else {
                    continue;
                };
                if txn
                    .inode_position(inode)
                    .map_err(|error| RecordError::Database(error.to_string()))?
                    .is_some()
                {
                    continue;
                }
                None
            }
            FileStatus::Added if historical_destinations.contains(&path) => {
                let Some(projected) = absent.get(&path) else {
                    continue;
                };
                Some(projected.inode)
            }
            _ => continue,
        };
        if used_new.contains(&path) || !root.join(&path).is_file() {
            continue;
        }
        let working_bytes = crate::content_filter::read_working_bytes(&root.join(&path))
            .map_err(|error| RecordError::Database(error.to_string()))?;
        let filtered = content_filter
            .clean(Path::new(&path), &working_bytes)
            .map_err(|error| RecordError::Database(error.to_string()))?;
        for warning in &filtered.warnings {
            log::warn!("record rename detection: {warning}");
        }
        destinations.push((path, filtered.bytes, expected_inode));
    }
    destinations.sort_by(|left, right| left.0.cmp(&right.0));

    let mut candidates = Vec::new();
    for entry in deleted {
        let old_path = entry.path().to_string_lossy().to_string();
        if used_old.contains(&old_path) {
            continue;
        }
        let Some(source) = present.get(&old_path) else {
            continue;
        };
        let old_content = load_old_content(source.inode, source.position)?;
        for (new_path, new_content, expected_inode) in &destinations {
            if expected_inode.is_some_and(|inode| inode != source.inode) {
                continue;
            }
            let (score, basis) = move_similarity(&old_content, new_content);
            if score >= PROBABLE_MOVE_THRESHOLD_BPS {
                candidates.push(Candidate {
                    old_path: old_path.clone(),
                    new_path: new_path.clone(),
                    inode: source.inode,
                    score,
                    basis,
                });
            }
        }
    }
    candidates.sort_by(|left, right| {
        (&left.old_path, &left.new_path).cmp(&(&right.old_path, &right.new_path))
    });

    let mut source_degree = std::collections::HashMap::<String, usize>::new();
    let mut destination_degree = std::collections::HashMap::<String, usize>::new();
    for candidate in &candidates {
        *source_degree.entry(candidate.old_path.clone()).or_default() += 1;
        *destination_degree
            .entry(candidate.new_path.clone())
            .or_default() += 1;
    }

    let mut unresolved = Vec::new();
    for candidate in candidates {
        let unique =
            source_degree[&candidate.old_path] == 1 && destination_degree[&candidate.new_path] == 1;
        if unique {
            planning.moves.push(make_plan(
                &candidate.old_path,
                &candidate.new_path,
                candidate.inode,
            )?);
            planning.evidence.insert_probable(ProbableMove::new(
                &candidate.old_path,
                &candidate.new_path,
                candidate.inode,
                candidate.score,
                candidate.basis,
            ));
            used_old.insert(candidate.old_path);
            used_new.insert(candidate.new_path);
        } else {
            planning
                .unresolved_destinations
                .insert(candidate.new_path.clone());
            unresolved.push(RenameCandidate::new(
                candidate.old_path,
                candidate.new_path,
                candidate.score,
                candidate.basis,
            ));
        }
    }
    if !unresolved.is_empty() {
        planning
            .evidence
            .insert_loss(LossNote::rename_unresolved(unresolved));
    }
    planning
        .fresh_additions
        .extend(historical_destinations.difference(&used_new).cloned());
    planning
        .moves
        .sort_by(|left, right| left.old_path.cmp(&right.old_path));
    Ok(planning)
}

fn move_similarity(old: &[u8], new: &[u8]) -> (u16, crate::record::MoveBasis) {
    use crate::record::MoveBasis;

    if old == new {
        return (10_000, MoveBasis::ByteIdentity);
    }
    if old.len() < 2 || new.len() < 2 {
        return (0, MoveBasis::ContentSimilarity);
    }

    // Sørensen-Dice similarity over byte-bigram multisets is deterministic,
    // encoding-agnostic, and remains stable across small insertions without the
    // quadratic cost of pairwise edit distance.
    let mut old_counts = vec![0_u32; 1 << 16];
    let mut new_counts = vec![0_u32; 1 << 16];
    for pair in old.windows(2) {
        old_counts[(usize::from(pair[0]) << 8) | usize::from(pair[1])] += 1;
    }
    for pair in new.windows(2) {
        new_counts[(usize::from(pair[0]) << 8) | usize::from(pair[1])] += 1;
    }
    let intersection: u64 = old_counts
        .iter()
        .zip(&new_counts)
        .map(|(left, right)| u64::from((*left).min(*right)))
        .sum();
    let total = (old.len() - 1 + new.len() - 1) as u64;
    let score = ((2 * intersection * 10_000) / total).min(10_000) as u16;
    (score, MoveBasis::ContentSimilarity)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NameResolutionPolicy {
    EditedContent,
    PreserveWorkingIdentity,
    ForeignMerge,
}

#[allow(clippy::too_many_arguments)]
fn prepare_name_resolution(
    txn: &atomic_core::pristine::ReadTxn,
    cached: &CachedGraphTxn<'_>,
    store: &ChangeStore,
    visibility: &GraphVisibilityClosure,
    path: &str,
    conflict: &super::name_resolution::ProjectedNameConflict,
    working: &[u8],
    policy: NameResolutionPolicy,
) -> Result<PreparedNameResolution, RecordError> {
    use atomic_core::record::workflow::globalize::{
        globalize_solve_name_conflict, GlobalizeContext, NameConflictClaim,
    };
    let preserve_mode = policy == NameResolutionPolicy::PreserveWorkingIdentity;

    if conflict.is_rename_conflict() {
        return Err(RecordError::Database(format!(
            "rename conflict at '{}' must be resolved by recording a surviving path, not by choosing file content",
            path
        )));
    }
    let sides = conflict.sides_at_path(path);
    if sides.len() < 2 || sides.iter().any(|side| side.is_directory()) {
        return Err(RecordError::Database(format!(
            "name conflict at '{}' cannot infer a unique file claimant",
            path
        )));
    }

    let mut external_hashes = std::collections::HashMap::new();
    for change_id in visibility.iter_dependency_first().copied() {
        if change_id.is_root() {
            continue;
        }
        let hash = txn
            .get_external(change_id)
            .map_err(|error| RecordError::Database(error.to_string()))?
            .ok_or_else(|| {
                RecordError::Database(format!(
                    "visible change {} has no external hash",
                    change_id.get()
                ))
            })?;
        external_hashes.insert(change_id, hash);
    }
    let inode_graph_table = txn
        .open_inode_graph_table()
        .map_err(|error| RecordError::Database(error.to_string()))?;
    let mut matching = Vec::new();
    for side in &sides {
        let content = super::materialize::render_name_conflict_side(
            txn,
            store,
            &inode_graph_table,
            visibility,
            &external_hashes,
            path,
            side.inode,
            side.position,
        )
        .map_err(RecordError::Database)?;

        if content == working {
            matching.push((*side).clone());
        }
    }
    if matching.is_empty() && !preserve_mode {
        // This is an explicit record of edited resolution bytes, not an
        // observation or a request to preserve one existing incarnation.
        // Keep a stable graph identity as the edit's base. Order by canonical
        // introducing hash and position, never local inode allocation, TREE,
        // previously selected view, or import order. The resolution operation
        // retains every losing claim as history and depends on all sides.
        let mut candidates = Vec::with_capacity(sides.len());
        for side in &sides {
            let hash = external_hashes.get(&side.position.change).ok_or_else(|| {
                RecordError::Database(format!(
                    "name conflict at '{path}' has no canonical identity for a claimant"
                ))
            })?;
            candidates.push(((*hash, side.position.pos), (*side).clone()));
        }
        candidates.sort_by_key(|(identity, _)| *identity);
        matching.push(candidates.remove(0).1);
    }
    if matching.len() > 1 && policy == NameResolutionPolicy::ForeignMerge {
        // A Git merge explicitly selects a single file at this path but does
        // not carry Atomic inode identities. When multiple claims have exactly
        // those bytes, choose a stable canonical identity for the explicit
        // SolveNameConflict operation. This does not deduplicate graph history
        // or use ambient TREE / local allocation order to hide a conflict.
        matching.sort_by_key(|side| (external_hashes[&side.position.change], side.position.pos));
        matching.truncate(1);
    }
    if matching.len() != 1 {
        // Review ::15 blocker support (user decision "Preserve current
        // files"): with MULTIPLE byte-equal claimants the working content is
        // ambiguous by bytes alone, so the EXPLICIT preserve-mode resolves
        // the identity through the RAW canonical TREE binding for the path
        // (the global path→inode row — distinct from the filtered conflict
        // projection). The binding must EXIST and correspond to one of the
        // byte-equal sides; otherwise the survivor's identity is unresolved
        // and the resolution FAILS CLOSED — a lowest-inode or any other
        // arbitrary fallback was never authorized (review ::26 N1 probe:
        // first=Inode(2) second=Inode(3) TREE=None, the old fallback picked
        // Inode(2)). A ZERO-match path is still refused: unrecorded working
        // content is never silently chosen.
        let canonical_selected = if preserve_mode {
            let tree_inode = txn
                .get_inode(path)
                .map_err(|e| RecordError::Database(e.to_string()))?;
            match tree_inode {
                Some(canonical_inode) => {
                    let canonical = matching
                        .iter()
                        .find(|side| side.inode == canonical_inode)
                        .cloned();
                    match canonical {
                        Some(side) => {
                            log::info!(
                                "record: preserve-content resolution at '{path}' selects the \
                                 canonical TREE-bound claimant (inode {}) among {} byte-equal sides",
                                side.inode.get(),
                                matching.len()
                            );
                            Some(side)
                        }
                        None => {
                            return Err(RecordError::Database(format!(
                                "preserve-content resolution refused at '{path}': the canonical \
                                 TREE binding (inode {}) is stale or unrelated — it does not match \
                                 any of the {} working-byte-equal claimants; re-bind the intended \
                                 incarnation (e.g. a normal record on its view) and retry",
                                canonical_inode.get(),
                                matching.len()
                            )));
                        }
                    }
                }
                None => {
                    return Err(RecordError::Database(format!(
                        "preserve-content resolution refused at '{path}': no canonical TREE \
                         binding exists for the path, so the survivor's identity is unresolved \
                         among {} working-byte-equal claimants; establish the binding (e.g. a \
                         normal record that binds the intended incarnation) and retry",
                        matching.len()
                    )));
                }
            }
        } else {
            None
        };
        match canonical_selected {
            Some(side) => {
                matching.clear();
                matching.push(side);
            }
            None => {
                return Err(RecordError::Database(format!(
                    "resolved name conflict at '{}' matches {} claimants; leave exactly one side byte-for-byte before recording",
                    path,
                    matching.len()
                )));
            }
        }
    }
    let winner = matching.remove(0);
    let losing_claims: Vec<_> = sides
        .iter()
        .filter(|side| side.inode != winner.inode)
        .flat_map(|side| {
            side.claims.iter().map(|claim| {
                NameConflictClaim::new(side.position, claim.parent, claim.name, claim.introduced_by)
            })
        })
        .collect();
    if losing_claims.is_empty() {
        return Err(RecordError::Database(format!(
            "name conflict at '{}' has no losing structural claims",
            path
        )));
    }

    let view_graph = ViewGraph::new(cached, visibility.clone());
    let mut context = GlobalizeContext::new(&view_graph);
    let operation =
        globalize_solve_name_conflict(&mut context, path, winner.position, losing_claims)?;
    let mut conflict_inodes: Vec<_> = conflict.sides.iter().map(|side| side.inode).collect();
    conflict_inodes.sort_by_key(|inode| inode.get());
    conflict_inodes.dedup();
    Ok(PreparedNameResolution {
        winner,
        operation,
        dependencies: context.dependencies_sorted(),
        conflict_inodes,
    })
}

fn visibility_before_deletions(
    txn: &atomic_core::pristine::ReadTxn,
    deletions: &[Hash],
) -> Result<GraphVisibilityClosure, RecordError> {
    if deletions.is_empty() {
        return Err(RecordError::Database(
            "undelete has no causally maximal deletion".to_string(),
        ));
    }

    let mut membership = Vec::new();
    for deletion in deletions {
        let deletion_id = txn
            .get_internal(deletion)
            .map_err(|error| RecordError::Database(error.to_string()))?
            .ok_or_else(|| {
                RecordError::Database(format!("undelete references unknown deletion {}", deletion))
            })?;
        for dependency in txn
            .get_indexed_change_deps(deletion_id)
            .map_err(|error| RecordError::Database(error.to_string()))?
        {
            let dependency_id = txn
                .get_internal(&dependency)
                .map_err(|error| RecordError::Database(error.to_string()))?
                .ok_or_else(|| {
                    RecordError::Database(format!(
                        "deletion {} has unregistered dependency {}",
                        deletion, dependency
                    ))
                })?;
            membership.push(dependency_id);
        }
    }
    let membership = ViewMembershipSet::from_ordered(membership);
    GraphVisibilityClosure::try_from_membership(txn, &membership)
        .map_err(|error| RecordError::Database(error.to_string()))
}

fn existing_crdt_identity(
    txn: &atomic_core::pristine::ReadTxn,
    inode: Inode,
    visibility: &GraphVisibilityClosure,
) -> Result<(atomic_core::crdt::TrunkId, Vec<atomic_core::crdt::BranchId>), RecordError> {
    use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
    use atomic_core::crdt::tables::{decode_trunk_id, encode_branch_id};
    use atomic_core::pristine::CrdtTxnT;

    let trunk_key = txn
        .get_crdt_inode_trunk(inode.get())
        .map_err(|error| RecordError::Database(error.to_string()))?
        .ok_or_else(|| {
            RecordError::Database(format!(
                "inode {} has no CRDT trunk identity required for identity-preserving lifecycle recording",
                inode.get()
            ))
        })?;
    let trunk = decode_trunk_id(&trunk_key);
    let mut branches = Vec::new();
    for branch in iter_trunk_branches_in_file_order(txn, trunk)
        .map_err(|error| RecordError::Database(error.to_string()))?
    {
        if !visibility.contains(branch.change_id()) {
            continue;
        }
        let key = encode_branch_id(&branch);
        if txn
            .get_crdt_branch(&key)
            .map_err(|error| RecordError::Database(error.to_string()))?
            .is_some_and(|data| data.state.is_alive())
        {
            branches.push(branch);
        }
    }
    Ok((trunk, branches))
}

/// Look up inode + position for a path using a shared read transaction.
fn get_inode_position(
    txn: &atomic_core::pristine::ReadTxn,
    path: &str,
) -> Result<(Inode, Position<NodeId>), String> {
    use atomic_core::pristine::TreeTxnT;
    let inode = txn
        .get_inode(path)
        .map_err(|e| format!("get_inode: {}", e))?
        .ok_or_else(|| format!("Inode not found for {}", path))?;
    let position = txn
        .inode_position(inode)
        .map_err(|e| format!("inode_position: {}", e))?
        .ok_or_else(|| format!("Position not found for {}", path))?;
    Ok((inode, position))
}
