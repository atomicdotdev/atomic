use std::collections::HashMap;
use std::time::Instant;

use atomic_core::apply::{apply_file_ops_batched_groups, ApplyFileOpsStats};
use atomic_core::change::{Atom, Change, FileOps, GraphOp, LineOps};
use atomic_core::crdt::tables::{encode_branch_id, encode_trunk_id, encode_vertex_position};
use atomic_core::crdt::{BranchId, BranchOp, LeafOp, TrunkId, TrunkOp};
use atomic_core::pristine::{CrdtTxnT, GraphTxnT, MutTxnT, ViewTxnT};
use atomic_core::types::{ChangePosition, GraphNode, NodeId};

use crate::apply::get_view_changes;
use crate::{Repository, RepositoryError};

/// Options for materializing stored FileOps into CRDT tables.
#[derive(Debug, Clone, Default)]
pub struct CrdtMaterializeOptions {
    /// View whose ordered changes should be replayed.
    pub view: Option<String>,
    /// Re-apply rows that appear to already have CRDT trunks.
    pub force: bool,
}

/// Summary from a CRDT materialization pass.
#[derive(Debug, Clone, Default)]
pub struct CrdtMaterializeOutcome {
    pub view: String,
    pub changes_scanned: usize,
    pub changes_applied: usize,
    pub file_ops_applied: usize,
    pub file_ops_already_materialized: usize,
    pub file_ops_skipped: usize,
    pub skip_stats: CrdtMaterializeSkipStats,
    pub skip_samples: Vec<String>,
    pub elapsed_ms: u128,
    pub stats: ApplyFileOpsStats,
}

/// Why FileOps were not materialized in the current phase-2 pass.
#[derive(Debug, Clone, Default)]
pub struct CrdtMaterializeSkipStats {
    pub non_create_trunk: usize,
    pub unresolved_path: usize,
    pub unresolved_line: usize,
    pub missing_content_range: usize,
    pub non_insert_branch: usize,
    pub non_insert_leaf: usize,
}

impl CrdtMaterializeSkipStats {
    pub fn total(&self) -> usize {
        self.non_create_trunk
            + self.unresolved_path
            + self.unresolved_line
            + self.missing_content_range
            + self.non_insert_branch
            + self.non_insert_leaf
    }
}

impl Repository {
    /// Populate CRDT tables from FileOps already stored in changes.
    ///
    /// This is phase 2 for graph-first Git import: phase 1 writes every graph
    /// vertex/edge and stores semantic operations in the change file; this pass
    /// fans out the subset of those operations that are already graph-linked and
    /// safe to index. Diff-only placeholder FileOps remain stored for review
    /// metadata but are skipped until they can be resolved against existing
    /// branch IDs.
    pub fn materialize_crdt_from_changes(
        &self,
        options: CrdtMaterializeOptions,
    ) -> Result<CrdtMaterializeOutcome, RepositoryError> {
        let view_name = options
            .view
            .clone()
            .unwrap_or_else(|| self.current_view.clone());
        let start = Instant::now();
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;

        let changes =
            get_view_changes(&txn, &view).map_err(|e| RepositoryError::Apply(e.to_string()))?;

        let mut outcome = CrdtMaterializeOutcome {
            view: view_name,
            changes_scanned: changes.len(),
            ..CrdtMaterializeOutcome::default()
        };

        let mut groups = Vec::new();
        let mut live_files: HashMap<String, MaterializedFileState> = HashMap::new();

        for (_seq, hash) in changes {
            let change_id = txn
                .get_internal(&hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: hash.to_string(),
                })?;
            let change = self.load_change(&hash)?;
            if !change.has_file_ops() {
                continue;
            }
            if !is_git_import_change(&change) {
                outcome.file_ops_already_materialized += change.file_ops().len();
                continue;
            }

            apply_git_path_metadata(&change, &mut live_files);
            let insertion_ranges = collect_insertion_ranges_by_path(&change);
            let mut next_branch_idx = next_change_branch_idx(change.file_ops());
            let mut safe_ops = Vec::new();
            for ops in change.file_ops() {
                if phase2_can_materialize_create_file_ops(ops) {
                    if !options.force && crdt_trunk_exists(&txn, change_id, ops)? {
                        seed_materialized_file(change_id, ops, &mut live_files);
                        outcome.file_ops_already_materialized += 1;
                        continue;
                    }
                    seed_materialized_file(change_id, ops, &mut live_files);
                    safe_ops.push(ops.clone());
                } else if ops.trunk_op().is_none() {
                    match resolve_existing_file_ops(
                        change_id,
                        ops,
                        insertion_ranges.get(ops.path()),
                        &mut live_files,
                        &mut next_branch_idx,
                    ) {
                        Ok(Some(resolved)) => {
                            if !options.force
                                && file_ops_already_materialized(&txn, change_id, &resolved)?
                            {
                                outcome.file_ops_already_materialized += 1;
                            } else {
                                safe_ops.push(resolved);
                            }
                        }
                        Ok(None) => {
                            record_skip(
                                &mut outcome,
                                ops.path(),
                                CrdtMaterializeSkipReason::NonCreateTrunk,
                            );
                        }
                        Err(reason) => {
                            record_skip(&mut outcome, ops.path(), reason);
                        }
                    }
                } else {
                    record_skip(
                        &mut outcome,
                        ops.path(),
                        CrdtMaterializeSkipReason::NonCreateTrunk,
                    );
                }
            }

            if safe_ops.is_empty() {
                continue;
            }
            outcome.file_ops_applied += safe_ops.len();
            outcome.changes_applied += 1;
            groups.push((change_id, safe_ops));
        }

        if !groups.is_empty() {
            outcome.stats = apply_file_ops_batched_groups(&mut txn, &groups)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        outcome.elapsed_ms = start.elapsed().as_millis();
        Ok(outcome)
    }
}

fn crdt_trunk_exists<T: CrdtTxnT>(
    txn: &T,
    change_id: NodeId,
    ops: &FileOps,
) -> Result<bool, RepositoryError> {
    let raw = ops.trunk_id();
    let trunk_id = if raw.change_id().is_root() {
        TrunkId::new(change_id, raw.file_idx())
    } else {
        raw
    };
    let key = encode_trunk_id(&trunk_id);
    txn.get_crdt_trunk(&key)
        .map(|v| v.is_some())
        .map_err(|e| RepositoryError::Database(e.to_string()))
}

fn file_ops_already_materialized<T: CrdtTxnT>(
    txn: &T,
    change_id: NodeId,
    ops: &FileOps,
) -> Result<bool, RepositoryError> {
    if ops.line_ops().is_empty() {
        return Ok(false);
    }

    for line in ops.line_ops() {
        let branch_id = line.branch_id();
        let branch_key = encode_branch_id(&branch_id);
        match line.operation() {
            BranchOp::Insert { .. } | BranchOp::Modify { .. } => {
                let Some((start, end)) = line.content_range() else {
                    return Ok(false);
                };
                let vertex_key = encode_vertex_position(&GraphNode {
                    change: change_id,
                    start,
                    end,
                });
                let mapped = txn
                    .get_crdt_vertex_branch(&vertex_key)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                if mapped != Some(branch_id) {
                    return Ok(false);
                }
            }
            BranchOp::Delete { .. } => {
                let branch = txn
                    .get_crdt_branch(&branch_key)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                if !branch.is_some_and(|branch| branch.state.is_deleted()) {
                    return Ok(false);
                }
            }
            BranchOp::Restore { .. } => {
                let branch = txn
                    .get_crdt_branch(&branch_key)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                if !branch.is_some_and(|branch| branch.state.is_alive()) {
                    return Ok(false);
                }
            }
            BranchOp::Reparent { .. } => return Ok(false),
        }
    }

    Ok(true)
}

fn record_skip(
    outcome: &mut CrdtMaterializeOutcome,
    path: &str,
    reason: CrdtMaterializeSkipReason,
) {
    outcome.skip_stats.record(reason);
    outcome.file_ops_skipped += 1;
    if outcome.skip_samples.len() < 16 {
        outcome.skip_samples.push(format!("{}:{:?}", path, reason));
    }
}

fn is_git_import_change(change: &Change) -> bool {
    change
        .unhashed
        .as_ref()
        .and_then(|value| value.get("git"))
        .is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrdtMaterializeSkipReason {
    NonCreateTrunk,
    UnresolvedPath,
    UnresolvedLine,
    MissingContentRange,
    NonInsertBranch,
    NonInsertLeaf,
}

impl CrdtMaterializeSkipStats {
    fn record(&mut self, reason: CrdtMaterializeSkipReason) {
        match reason {
            CrdtMaterializeSkipReason::NonCreateTrunk => self.non_create_trunk += 1,
            CrdtMaterializeSkipReason::UnresolvedPath => self.unresolved_path += 1,
            CrdtMaterializeSkipReason::UnresolvedLine => self.unresolved_line += 1,
            CrdtMaterializeSkipReason::MissingContentRange => self.missing_content_range += 1,
            CrdtMaterializeSkipReason::NonInsertBranch => self.non_insert_branch += 1,
            CrdtMaterializeSkipReason::NonInsertLeaf => self.non_insert_leaf += 1,
        }
    }
}

#[derive(Debug, Clone)]
struct MaterializedFileState {
    trunk_id: TrunkId,
    branches: Vec<BranchId>,
}

type InsertionRangeMap = HashMap<String, HashMap<usize, Vec<(ChangePosition, ChangePosition)>>>;

fn phase2_can_materialize_create_file_ops(ops: &FileOps) -> bool {
    phase2_create_skip_reason(ops).is_none()
}

fn phase2_create_skip_reason(ops: &FileOps) -> Option<CrdtMaterializeSkipReason> {
    if !matches!(ops.trunk_op(), Some(TrunkOp::Create { .. })) {
        return Some(CrdtMaterializeSkipReason::NonCreateTrunk);
    }

    for line in ops.line_ops() {
        if line.content_range().is_none() {
            return Some(CrdtMaterializeSkipReason::MissingContentRange);
        }
        let BranchOp::Insert { content, .. } = line.operation() else {
            return Some(CrdtMaterializeSkipReason::NonInsertBranch);
        };
        if !content
            .iter()
            .all(|leaf| matches!(leaf, LeafOp::Insert { .. }))
        {
            return Some(CrdtMaterializeSkipReason::NonInsertLeaf);
        }
    }

    None
}

fn next_change_branch_idx(file_ops: &[FileOps]) -> u32 {
    file_ops
        .iter()
        .flat_map(FileOps::line_ops)
        .map(|line| line.branch_id().branch_idx())
        .max()
        .map_or(0, |idx| idx.saturating_add(1))
}

fn resolve_trunk_id(change_id: NodeId, trunk_id: TrunkId) -> TrunkId {
    if trunk_id.change_id().is_root() {
        TrunkId::new(change_id, trunk_id.file_idx())
    } else {
        trunk_id
    }
}

fn seed_materialized_file(
    change_id: NodeId,
    ops: &FileOps,
    live_files: &mut HashMap<String, MaterializedFileState>,
) {
    let trunk_id = resolve_trunk_id(change_id, ops.trunk_id());
    let branches = ops
        .line_ops()
        .iter()
        .map(|line| {
            let branch = line.branch_id();
            if branch.change_id().is_root() {
                BranchId::new(change_id, branch.branch_idx())
            } else {
                branch
            }
        })
        .collect();
    live_files.insert(
        ops.path().to_string(),
        MaterializedFileState { trunk_id, branches },
    );
}

fn collect_insertion_ranges_by_path(change: &Change) -> InsertionRangeMap {
    let mut ranges: InsertionRangeMap = HashMap::new();
    for hunk in change.hunks() {
        match hunk {
            GraphOp::Edit {
                change: Atom::Insertion(insertion),
                local,
                ..
            } => {
                ranges
                    .entry(local.path.clone())
                    .or_default()
                    .entry(local.line as usize)
                    .or_default()
                    .push((insertion.start, insertion.end));
            }
            GraphOp::Replacement {
                replacement, local, ..
            } => {
                ranges
                    .entry(local.path.clone())
                    .or_default()
                    .entry(local.line as usize)
                    .or_default()
                    .push((replacement.start, replacement.end));
            }
            _ => {}
        }
    }
    ranges
}

fn apply_git_path_metadata(
    change: &Change,
    live_files: &mut HashMap<String, MaterializedFileState>,
) {
    let Some(diff_files) = change
        .unhashed
        .as_ref()
        .and_then(|value| value.get("git"))
        .and_then(|git| git.get("diff_lines"))
        .and_then(|diff_lines| diff_lines.as_array())
    else {
        return;
    };

    for file in diff_files {
        let operation = file.get("operation").and_then(|v| v.as_str());
        let path = file.get("path").and_then(|v| v.as_str());
        let old_path = file.get("old_path").and_then(|v| v.as_str());
        match (operation, old_path, path) {
            (Some("renamed"), Some(old_path), Some(path)) => {
                if let Some(state) = live_files.remove(old_path) {
                    live_files.insert(path.to_string(), state);
                }
            }
            (Some("copied"), Some(old_path), Some(path)) => {
                if let Some(state) = live_files.get(old_path).cloned() {
                    live_files.insert(path.to_string(), state);
                }
            }
            _ => {}
        }
    }
}

fn take_insertion_range(
    ranges: &mut Option<HashMap<usize, Vec<(ChangePosition, ChangePosition)>>>,
    line: usize,
) -> Option<(ChangePosition, ChangePosition)> {
    let line_ranges = ranges.as_mut()?.get_mut(&line)?;
    if line_ranges.is_empty() {
        None
    } else {
        Some(line_ranges.remove(0))
    }
}

fn resolve_existing_file_ops(
    change_id: NodeId,
    ops: &FileOps,
    insertion_ranges: Option<&HashMap<usize, Vec<(ChangePosition, ChangePosition)>>>,
    live_files: &mut HashMap<String, MaterializedFileState>,
    next_branch_idx: &mut u32,
) -> Result<Option<FileOps>, CrdtMaterializeSkipReason> {
    let Some(current) = live_files.get(ops.path()).cloned() else {
        return Err(CrdtMaterializeSkipReason::UnresolvedPath);
    };

    let mut branches = current.branches;
    let mut resolved = FileOps::edit(current.trunk_id, ops.path().to_string());
    let mut ranges = insertion_ranges.cloned();
    let mut line_offset: isize = 0;

    for line in ops.line_ops() {
        match line.operation() {
            BranchOp::Delete { content, .. } => {
                let old_line = line
                    .old_line_num()
                    .ok_or(CrdtMaterializeSkipReason::UnresolvedLine)?;
                let idx = adjusted_line_index(old_line, line_offset, branches.len())?;
                let branch_id = branches.remove(idx);
                resolved.add_line_op(LineOps::delete(branch_id, content.clone()));
                line_offset -= 1;
            }
            BranchOp::Insert { content, .. } => {
                let new_line = line
                    .new_line_num()
                    .ok_or(CrdtMaterializeSkipReason::UnresolvedLine)?;
                let (start, end) = take_insertion_range(&mut ranges, new_line)
                    .ok_or(CrdtMaterializeSkipReason::MissingContentRange)?;
                let idx = new_line.saturating_sub(1).min(branches.len());
                let after = if idx == 0 {
                    None
                } else {
                    Some(branches[idx - 1])
                };
                let branch_id = BranchId::new(change_id, *next_branch_idx);
                *next_branch_idx = next_branch_idx.saturating_add(1);
                let op = LineOps::insert(branch_id, after, content.clone())
                    .with_new_line_num(new_line)
                    .with_content_range(start, end);
                resolved.add_line_op(op);
                branches.insert(idx, branch_id);
                line_offset += 1;
            }
            BranchOp::Modify {
                old_content,
                new_content,
                ..
            } => {
                let old_line = line
                    .old_line_num()
                    .ok_or(CrdtMaterializeSkipReason::UnresolvedLine)?;
                let new_line = line
                    .new_line_num()
                    .ok_or(CrdtMaterializeSkipReason::UnresolvedLine)?;
                let idx = adjusted_line_index(old_line, line_offset, branches.len())?;
                let branch_id = branches[idx];
                let (start, end) = take_insertion_range(&mut ranges, new_line)
                    .ok_or(CrdtMaterializeSkipReason::MissingContentRange)?;
                let op = LineOps::modify(branch_id, old_content.clone(), new_content.clone())
                    .with_old_line_num(old_line)
                    .with_new_line_num(new_line)
                    .with_content_range(start, end);
                resolved.add_line_op(op);
            }
            BranchOp::Restore { .. } | BranchOp::Reparent { .. } => {
                return Err(CrdtMaterializeSkipReason::NonInsertBranch);
            }
        }
    }

    if resolved.line_ops().is_empty() {
        return Ok(None);
    }

    live_files.insert(
        ops.path().to_string(),
        MaterializedFileState {
            trunk_id: current.trunk_id,
            branches,
        },
    );
    Ok(Some(resolved))
}

fn adjusted_line_index(
    one_based_line: usize,
    line_offset: isize,
    len: usize,
) -> Result<usize, CrdtMaterializeSkipReason> {
    let base = one_based_line
        .checked_sub(1)
        .ok_or(CrdtMaterializeSkipReason::UnresolvedLine)?;
    let adjusted = base as isize + line_offset;
    if adjusted < 0 {
        return Err(CrdtMaterializeSkipReason::UnresolvedLine);
    }
    let idx = adjusted as usize;
    if idx >= len {
        return Err(CrdtMaterializeSkipReason::UnresolvedLine);
    }
    Ok(idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Encoding, LineOps};
    use atomic_core::crdt::{BranchId, LeafId};
    use atomic_core::diff::TokenKind;

    #[test]
    fn materialize_filter_accepts_graph_linked_create_inserts() {
        let mut ops = FileOps::create(
            TrunkId::new(NodeId::ROOT, 0),
            "src/lib.rs".to_string(),
            Some(Encoding::Utf8),
        );
        ops.add_line_op(
            LineOps::insert(
                BranchId::new(NodeId::ROOT, 0),
                None,
                vec![LeafOp::Insert {
                    after: Some(LeafId::new(NodeId::ROOT, 0)),
                    kind: TokenKind::Word,
                    content: b"fn".to_vec(),
                }],
            )
            .with_content_range(0usize.into(), 3usize.into()),
        );

        assert_eq!(phase2_create_skip_reason(&ops), None);
    }

    #[test]
    fn materialize_filter_rejects_diff_only_edit_ops() {
        let mut ops = FileOps::edit(TrunkId::new(NodeId::ROOT, 0), "src/lib.rs".to_string());
        ops.add_line_op(LineOps::insert(
            BranchId::new(NodeId::ROOT, 0),
            None,
            vec![LeafOp::Insert {
                after: None,
                kind: TokenKind::Word,
                content: b"fn".to_vec(),
            }],
        ));

        assert_eq!(
            phase2_create_skip_reason(&ops),
            Some(CrdtMaterializeSkipReason::NonCreateTrunk)
        );
    }
}
