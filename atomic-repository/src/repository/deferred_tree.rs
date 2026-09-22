use super::*;

use crate::tracking::{
    TreeProjectionError, TreeProjectionKind, TreeProjectionOperation, TreeProjectionPlan,
};
use atomic_core::operation::WorkingCopyStateRef;
use atomic_core::pristine::{
    PathClaimMutTxnT, PathClaimTxnT, WorkingCopyMutTxnT, WorkingCopyRecord, WorkingCopyTxnT,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;

const DEFERRED_TREE_JOURNAL: &str = "deferred-tree-ops.json";
const DEFERRED_TREE_JOURNAL_VERSION: u32 = 1;
const DEFERRED_TREE_ALIGNMENT_PENDING: &str = "deferred-tree-alignment.pending";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum DeferredTreeAction {
    Set {
        path: String,
    },
    Delete,
    /// Unbind `path` from the inode's desired state — a name-conflict
    /// resolution that surrenders the name keeps the inode's identity
    /// (and its REV_TREE claim) while ceasing to occupy the path.
    UnlinkName {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct DeferredTreeOp {
    /// Change whose visibility activates this TREE operation.
    pub(super) change: Hash,
    /// Stable inode position, stored with an external change hash so the
    /// journal never depends on process-local lookup state.
    pub(super) inode: Position<Hash>,
    /// Path visible before this inode's first journaled operation. Only the
    /// first event for an inode uses this baseline; later events are selected
    /// solely by change visibility.
    baseline_path: Option<String>,
    /// Stable node kind carried by operation metadata. Legacy journals may not
    /// contain it and must prove the kind through DIRECTORIES instead.
    #[serde(default)]
    pub(super) directory: Option<bool>,
    pub(super) action: DeferredTreeAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeferredTreeJournal {
    version: u32,
    #[serde(default)]
    ops: Vec<DeferredTreeOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingTreeAlignment {
    version: u32,
    source_view: String,
    target_view: String,
}

impl Default for DeferredTreeJournal {
    fn default() -> Self {
        Self {
            version: DEFERRED_TREE_JOURNAL_VERSION,
            ops: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct DesiredTreePath {
    desired_path: Option<String>,
    last_present_path: Option<String>,
    known_paths: HashSet<String>,
    deleted: bool,
    maximal_changes: Vec<Hash>,
    /// Concurrent journal actions disagree. This field is retained only for
    /// legacy journal validation; PATH_CLAIMS owns canonical projection.
    ambiguous: bool,
    directory: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectedAbsent {
    pub(super) path: String,
    pub(super) inode: Inode,
    pub(super) position: Position<NodeId>,
    pub(super) directory: bool,
    pub(super) deleted_by: Vec<Hash>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TreeProjection {
    pub(super) present: HashMap<String, OutputItem>,
    pub(super) present_metadata: HashMap<String, super::name_resolution::ProjectedPathClaim>,
    pub(super) absent: Vec<MaterializedEntry>,
    pub(super) absent_metadata: HashMap<String, ProjectedAbsent>,
    pub(super) name_conflicts: HashMap<String, super::name_resolution::ProjectedNameConflict>,
}

fn projected_parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn projection_error(error: TreeProjectionError) -> RepositoryError {
    match error {
        TreeProjectionError::Database(message) => RepositoryError::Database(message),
        TreeProjectionError::IncompleteMetadata(message)
        | TreeProjectionError::Conflict(message) => RepositoryError::InvalidOperation { message },
    }
}

fn desired_tree_paths<F>(
    ops: &[DeferredTreeOp],
    visible_changes: &HashSet<Hash>,
    mut depends_on: F,
) -> Result<HashMap<Position<Hash>, DesiredTreePath>, RepositoryError>
where
    F: FnMut(Hash, Hash) -> Result<bool, RepositoryError>,
{
    let mut by_inode: HashMap<Position<Hash>, Vec<(usize, &DeferredTreeOp)>> = HashMap::new();
    for (order, op) in ops.iter().enumerate() {
        by_inode.entry(op.inode).or_default().push((order, op));
    }

    let mut desired = HashMap::new();
    for (inode, inode_ops) in by_inode {
        let baseline = inode_ops
            .iter()
            .find_map(|(_, op)| op.baseline_path.clone());
        let mut known_paths: HashSet<String> = inode_ops
            .iter()
            .filter_map(|(_, op)| op.baseline_path.clone())
            .collect();
        let mut directory = None;
        for (_, op) in &inode_ops {
            if let DeferredTreeAction::Set { path } = &op.action {
                known_paths.insert(path.clone());
            }
            if let Some(candidate) = op.directory {
                if directory.is_some_and(|existing| existing != candidate) {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection metadata changes inode {} from file to directory",
                            inode.pos.get()
                        ),
                    });
                }
                directory = Some(candidate);
            }
        }

        let visible: Vec<(usize, &DeferredTreeOp)> = inode_ops
            .into_iter()
            .filter(|(_, op)| visible_changes.contains(&op.change))
            .collect();
        let mut maximal = Vec::new();
        for (index, candidate) in &visible {
            let mut superseded = false;
            for (other_index, other) in &visible {
                if index == other_index {
                    continue;
                }
                if (candidate.change == other.change && other_index > index)
                    || (candidate.change != other.change
                        && depends_on(other.change, candidate.change)?)
                {
                    superseded = true;
                    break;
                }
            }
            if !superseded {
                maximal.push((*index, *candidate));
            }
        }

        let maximal_changes: Vec<Hash> = maximal.iter().map(|(_, op)| op.change).collect();
        let (desired_path, last_present_path, deleted, ambiguous) = if maximal.is_empty() {
            (baseline.clone(), baseline, false, false)
        } else {
            let first = &maximal[0].1.action;
            if maximal.iter().any(|(_, op)| &op.action != first) {
                // The legacy journal validator must not invent an order for
                // concurrent actions. Canonical projection is performed from
                // PATH_CLAIMS and never consumes this compatibility value.
                (None, baseline.clone(), false, true)
            } else {
                match first {
                    DeferredTreeAction::Set { path } => {
                        (Some(path.clone()), Some(path.clone()), false, false)
                    }
                    DeferredTreeAction::UnlinkName { path } => {
                        // A name-conflict resolution that surrenders the name
                        // keeps the inode's identity while vacating the path.
                        (None, Some(path.clone()), true, false)
                    }
                    DeferredTreeAction::Delete => {
                        let mut predecessors = Vec::new();
                        for (set_index, set_op) in &visible {
                            if !matches!(set_op.action, DeferredTreeAction::Set { .. }) {
                                continue;
                            }
                            let mut precedes_delete = false;
                            for (delete_index, delete_op) in &maximal {
                                if (set_op.change == delete_op.change && set_index < delete_index)
                                    || (set_op.change != delete_op.change
                                        && depends_on(delete_op.change, set_op.change)?)
                                {
                                    precedes_delete = true;
                                    break;
                                }
                            }
                            if precedes_delete {
                                predecessors.push((*set_index, *set_op));
                            }
                        }
                        let mut maximal_predecessors = Vec::new();
                        for (index, candidate) in &predecessors {
                            let mut superseded = false;
                            for (other_index, other) in &predecessors {
                                if index == other_index {
                                    continue;
                                }
                                if (candidate.change == other.change && other_index > index)
                                    || (candidate.change != other.change
                                        && depends_on(other.change, candidate.change)?)
                                {
                                    superseded = true;
                                    break;
                                }
                            }
                            if !superseded {
                                maximal_predecessors.push(*candidate);
                            }
                        }
                        let prior_paths: HashSet<String> = maximal_predecessors
                            .iter()
                            .filter_map(|op| match &op.action {
                                DeferredTreeAction::Set { path } => Some(path.clone()),
                                DeferredTreeAction::Delete
                                | DeferredTreeAction::UnlinkName { .. } => None,
                            })
                            .collect();
                        if prior_paths.len() > 1 {
                            return Err(RepositoryError::InvalidOperation {
                                message: format!(
                                    "delete for inode {} has causally ambiguous prior paths",
                                    inode.pos.get()
                                ),
                            });
                        }
                        (
                            None,
                            prior_paths.into_iter().next().or(baseline),
                            true,
                            false,
                        )
                    }
                }
            }
        };

        desired.insert(
            inode,
            DesiredTreePath {
                desired_path,
                last_present_path,
                known_paths,
                deleted,
                maximal_changes,
                ambiguous,
                directory,
            },
        );
    }

    // Keep every journal claimant for compatibility validation. This result is
    // never projected into TREE; durable PATH_CLAIMS performs that reduction.
    Ok(desired)
}

fn change_depends_on<T: GraphTxnT>(
    txn: &T,
    descendant: Hash,
    ancestor: Hash,
    memo: &mut HashMap<(Hash, Hash), bool>,
) -> Result<bool, RepositoryError> {
    if descendant == ancestor {
        return Ok(true);
    }
    if let Some(result) = memo.get(&(descendant, ancestor)) {
        return Ok(*result);
    }
    let descendant_id = txn
        .get_internal(&descendant)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "tree projection references unknown change {}",
                descendant.to_base32()
            ),
        })?;
    // Legacy repositories predate the dependency index. Never mistake an
    // unindexed change for one with zero dependencies — fall back to the
    // change file (dev #196/#206 parity, exercised by the unrecord-safety
    // legacy fixtures).
    // Legacy repositories predate the dependency index. Never mistake an
    // unindexed change for one with zero dependencies. change_depends_on
    // only has GraphTxnT (no change-store access), so unindexed changes
    // are treated as leaves here; callers that need the full closure
    // resolve through the repository's load path (see
    // check_unrecord_safety, dev #196/#206 parity).
    let dependencies = txn.get_indexed_change_deps(descendant_id).unwrap_or_default();
    for dependency in dependencies {
        if dependency == ancestor || change_depends_on(txn, dependency, ancestor, memo)? {
            memo.insert((descendant, ancestor), true);
            return Ok(true);
        }
    }
    memo.insert((descendant, ancestor), false);
    Ok(false)
}

fn desired_tree_paths_for_txn<T: GraphTxnT>(
    txn: &T,
    ops: &[DeferredTreeOp],
    visible_changes: &HashSet<Hash>,
) -> Result<HashMap<Position<Hash>, DesiredTreePath>, RepositoryError> {
    let mut memo = HashMap::new();
    desired_tree_paths(ops, visible_changes, |descendant, ancestor| {
        change_depends_on(txn, descendant, ancestor, &mut memo)
    })
}

fn causally_order_tree_ops<T: GraphTxnT>(
    txn: &T,
    ops: &[DeferredTreeOp],
) -> Result<Vec<DeferredTreeOp>, RepositoryError> {
    fn visit<T: GraphTxnT>(
        txn: &T,
        change: Hash,
        groups: &HashMap<Hash, Vec<DeferredTreeOp>>,
        visiting: &mut HashSet<Hash>,
        visited: &mut HashSet<Hash>,
        ordered: &mut Vec<DeferredTreeOp>,
    ) -> Result<(), RepositoryError> {
        if visited.contains(&change) {
            return Ok(());
        }
        if !visiting.insert(change) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "deferred TREE lifecycle contains a dependency cycle at {}",
                    change.to_base32()
                ),
            });
        }

        let change_id = txn
            .get_internal(&change)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "deferred TREE lifecycle references unknown change {}",
                    change.to_base32()
                ),
            })?;
        // Raw dependency rows: legacy repositories predate the index and the
        // indexed read fails closed on them (dev #196/#206 parity). Raw rows
        // are populated for indexed changes and empty for unindexed ones —
        // exact for cycle detection.
        for dependency in txn
            .get_change_deps(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            if groups.contains_key(&dependency) {
                visit(txn, dependency, groups, visiting, visited, ordered)?;
            }
        }

        visiting.remove(&change);
        visited.insert(change);
        ordered.extend(
            groups
                .get(&change)
                .expect("visited deferred TREE change has an operation group")
                .iter()
                .cloned(),
        );
        Ok(())
    }

    let mut group_order = Vec::new();
    let mut groups: HashMap<Hash, Vec<DeferredTreeOp>> = HashMap::new();
    for op in ops {
        if !groups.contains_key(&op.change) {
            group_order.push(op.change);
        }
        groups.entry(op.change).or_default().push(op.clone());
    }

    let mut ordered = Vec::with_capacity(ops.len());
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for change in group_order {
        visit(
            txn,
            change,
            &groups,
            &mut visiting,
            &mut visited,
            &mut ordered,
        )?;
    }
    Ok(ordered)
}

fn external_inode_position<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    inode: Inode,
) -> Result<Position<Hash>, RepositoryError> {
    let position = txn
        .inode_position(inode)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("inode {} has no graph position", inode.get()),
        })?;
    if position.change.is_root() {
        return Err(RepositoryError::InvalidOperation {
            message: format!("inode {} resolves to the ROOT change", inode.get()),
        });
    }
    let change = txn
        .get_external(position.change)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "inode {} references change {} without an external hash",
                inode.get(),
                position.change.get()
            ),
        })?;
    Ok(Position::new(change, position.pos))
}

fn push_unique(ops: &mut Vec<DeferredTreeOp>, op: DeferredTreeOp) {
    if !ops.iter().any(|existing| {
        existing.change == op.change && existing.inode == op.inode && existing.action == op.action
    }) {
        ops.push(op);
    }
}

fn external_position(
    change_hash: Hash,
    position: Position<Option<Hash>>,
) -> Result<Position<Hash>, RepositoryError> {
    let change = match position.change {
        None => change_hash,
        Some(hash) if hash == Hash::NONE => {
            return Err(RepositoryError::InvalidOperation {
                message: "tree operation inode cannot reference ROOT".to_string(),
            });
        }
        Some(hash) => hash,
    };
    Ok(Position::new(change, position.pos))
}

fn current_path_for_position<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    position: Position<Hash>,
) -> Result<Option<String>, RepositoryError> {
    let internal_change = txn
        .get_internal(&position.change)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "tree operation references unknown change {}",
                position.change.to_base32()
            ),
        })?;
    let inode = txn
        .position_inode(Position::new(internal_change, position.pos))
        .map_err(|e| RepositoryError::Database(e.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("tree operation position {} has no inode", position),
        })?;
    txn.get_path(inode)
        .map_err(|e| RepositoryError::Database(e.to_string()))
}

fn push_delete_for_path<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    change: Hash,
    path: &str,
    ops: &mut Vec<DeferredTreeOp>,
) -> Result<(), RepositoryError> {
    let Some(inode) = txn
        .get_inode(path)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(());
    };
    let position = external_inode_position(txn, inode)?;
    let op = DeferredTreeOp {
        change,
        inode: position,
        baseline_path: Some(path.to_string()),
        directory: Some(
            txn.is_directory(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?,
        ),
        action: DeferredTreeAction::Delete,
    };
    push_unique(ops, op);
    Ok(())
}

/// Collect graph-backed lifecycle metadata for the canonical tree projection.
/// Recording every add, delete, move, and undelete lets native record, import,
/// insert, and deferred replay derive the same cache state from visibility.
fn push_occupant_baseline<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    activating_change: Hash,
    path: &str,
    exclude: Option<Position<Hash>>,
    visible_on_target: Option<&HashSet<Hash>>,
    ops: &mut Vec<DeferredTreeOp>,
) -> Result<(), RepositoryError> {
    let Some(inode) = txn
        .get_inode(path)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(());
    };
    let Ok(position) = external_inode_position(txn, inode) else {
        return Ok(());
    };
    if Some(position) == exclude {
        return Ok(());
    }
    if visible_on_target.is_some_and(|visible| visible.contains(&position.change)) {
        // The current occupant belongs to the target view; it is a real owner,
        // not a foreign draft binding. Never manufacture a reverse delete.
        return Ok(());
    }
    push_unique(
        ops,
        DeferredTreeOp {
            change: activating_change,
            inode: position,
            baseline_path: Some(path.to_string()),
            directory: None,
            action: DeferredTreeAction::Delete,
        },
    );
    Ok(())
}

/// Collect view-scoped TREE lifecycle events before applying hunk-level TREE
/// mutations. Recording all adds, moves, and deletes, not just deferred Git
/// imports, keeps replay baselines valid when a later foreground change moves
/// the same inode on another view.
pub(super) fn collect_tree_ops<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    change_hash: Hash,
    change: &Change,
    deleted_paths: &[String],
    visible_on_target: Option<&HashSet<Hash>>,
) -> Result<Vec<DeferredTreeOp>, RepositoryError> {
    let mut ops = Vec::new();

    for graph_op in change.hunks() {
        match graph_op {
            GraphOp::FileAdd {
                add_inode, path, ..
            }
            | GraphOp::DirAdd {
                add_inode, path, ..
            } => {
                let added_position = Position::new(change_hash, add_inode.start);
                let directory = matches!(graph_op, GraphOp::DirAdd { .. });
                push_occupant_baseline(
                    txn,
                    change_hash,
                    path,
                    Some(added_position),
                    visible_on_target,
                    &mut ops,
                )?;
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode: added_position,
                        baseline_path: None,
                        directory: Some(directory),
                        action: DeferredTreeAction::Set { path: path.clone() },
                    },
                );
            }
            GraphOp::FileMove { add, path, .. } => {
                let external_position = external_position(change_hash, add.inode)?;
                push_occupant_baseline(
                    txn,
                    change_hash,
                    path,
                    Some(external_position),
                    visible_on_target,
                    &mut ops,
                )?;
                let op = DeferredTreeOp {
                    change: change_hash,
                    inode: external_position,
                    baseline_path: current_path_for_position(txn, external_position)?,
                    directory: Some(false),
                    action: DeferredTreeAction::Set { path: path.clone() },
                };
                // Keep the event even when TREE already contains `path`.
                // `atomic move` updates tracking before the subsequent record,
                // and that recorded change must still supersede an older
                // deferred rename for views where it is visible.
                push_unique(&mut ops, op);
            }
            GraphOp::SolveNameConflict { name, path } => {
                if let Ok(inode) = external_position(change_hash, name.inode) {
                    push_unique(
                        &mut ops,
                        DeferredTreeOp {
                            change: change_hash,
                            inode,
                            baseline_path: current_path_for_position(txn, inode)?
                                .or_else(|| Some(path.clone())),
                            directory: None,
                            action: if name.edges.is_empty() {
                                DeferredTreeAction::Set { path: path.clone() }
                            } else {
                                DeferredTreeAction::UnlinkName { path: path.clone() }
                            },
                        },
                    );
                }
            }
            GraphOp::FileDel { del, path, .. } | GraphOp::DirDel { del, path } => {
                let inode = external_position(change_hash, del.inode)?;
                let directory = matches!(graph_op, GraphOp::DirDel { .. });
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode,
                        baseline_path: Some(path.clone()),
                        directory: Some(directory),
                        action: DeferredTreeAction::Delete,
                    },
                );
            }
            GraphOp::FileUndel { undel, path, .. } | GraphOp::DirUndel { undel, path } => {
                let inode = external_position(change_hash, undel.inode)?;
                let directory = matches!(graph_op, GraphOp::DirUndel { .. });
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode,
                        baseline_path: current_path_for_position(txn, inode)?,
                        directory: Some(directory),
                        action: DeferredTreeAction::Set { path: path.clone() },
                    },
                );
            }
            GraphOp::Edit {
                change: atomic_core::change::Atom::EdgeUpdate(delete),
                local,
                ..
            }
            | GraphOp::Replacement {
                change: delete,
                local,
                ..
            } if deleted_paths.iter().any(|path| path == &local.path) => {
                let inode = external_position(change_hash, delete.inode)?;
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode,
                        baseline_path: Some(local.path.clone()),
                        directory: Some(false),
                        action: DeferredTreeAction::Delete,
                    },
                );
            }
            _ => {}
        }
    }

    // Some import deletion paths are represented as content replacements,
    // not FileDel hunks, so preserve their TREE intent explicitly as well.
    for path in deleted_paths {
        let already_resolved = ops.iter().any(|op| {
            matches!(op.action, DeferredTreeAction::Delete)
                && op.baseline_path.as_deref() == Some(path.as_str())
        });
        if !already_resolved {
            push_delete_for_path(txn, change_hash, path, &mut ops)?;
        }
    }

    Ok(ops)
}

#[derive(Debug, Clone)]
pub(super) struct PreparedTreeProjection {
    ops: Vec<DeferredTreeOp>,
    prerequisites: TreeProjectionPlan,
    change_id: NodeId,
    change: Change,
}

impl PreparedTreeProjection {
    pub(super) fn apply_prerequisites<T: MutTxnT>(
        &self,
        txn: &mut T,
    ) -> Result<(), RepositoryError> {
        self.prerequisites.apply(txn).map_err(projection_error)
    }
}

/// Materialize explicit name selections into TREE using stable graph identity.
/// Recording and cross-view insertion share this operation; no content is copied
/// (dev #203: name conflicts resolve as namespace patches).
#[allow(dead_code)] // dev #203 entry point; ours' PATH_CLAIMS projection supersedes
pub(super) fn apply_name_selections<T: MutTxnT>(
    txn: &mut T,
    change_id: NodeId,
    change: &Change,
) -> Result<(), RepositoryError> {
    for op in change.hunks() {
        let GraphOp::SolveNameConflict { name, path } = op else {
            continue;
        };
        if !name.edges.is_empty() {
            continue;
        }
        let unresolved = || RepositoryError::InvalidOperation {
            message: format!("cannot resolve retained identity for {path}: name conflict"),
        };
        let inode_change = match name.inode.change {
            Some(hash) => txn
                .get_internal(&hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(unresolved)?,
            None => change_id,
        };
        let inode = txn
            .position_inode(Position::new(inode_change, name.inode.pos))
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(unresolved)?;
        txn.put_tree(path, inode)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
    }
    Ok(())
}

impl Repository {
    /// The explicit EmptyDirectory loss facts for a view's Atomic→Git
    /// projection (review CB-9C R7): every explicitly tracked directory whose
    /// projection contains no alive file beneath it. Git trees cannot hold an
    /// empty directory, so the projection omits it — the loss note is the
    /// reviewed fact that it was omitted deliberately, not silently. Git
    /// import must never fabricate directory-only inodes to avoid this loss.
    ///
    /// # Errors
    ///
    /// Returns `RepositoryError` when the projection or visibility closure
    /// fails.
    pub fn empty_directory_loss_notes(
        &self,
        view_name: &str,
    ) -> Result<Vec<crate::record::LossNote>, RepositoryError> {
        use atomic_core::pristine::ViewTxnT;
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
        let visibility = super::filter::graph_visibility_closure(&txn, &view)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        // A tracked directory is empty exactly when no present FILE path
        // carries it as a prefix (the projection itself only materializes
        // structural directories from file paths).
        let mut notes = Vec::new();
        let mut directories: Vec<&str> = projection
            .present
            .iter()
            .filter(|(path, item)| {
                item.is_directory
                    && !crate::repository::project_tree::atomic_private_path(path.as_bytes())
            })
            .map(|(path, _)| path.as_str())
            .collect();
        directories.sort_unstable();
        for directory in directories {
            let prefix = format!("{directory}/");
            let holds_files = projection.present.iter().any(|(path, item)| {
                !item.is_directory
                    && !crate::repository::project_tree::atomic_private_path(path.as_bytes())
                    && path.as_str().starts_with(&prefix)
            });
            if !holds_files {
                notes.push(crate::record::LossNote::empty_directory(
                    directory.to_string(),
                ));
            }
        }
        Ok(notes)
    }

    pub(super) fn plan_tree_projection(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        change_id: NodeId,
        change_hash: Hash,
        change: &Change,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
    ) -> Result<PreparedTreeProjection, RepositoryError> {
        let ops = collect_tree_ops(&*txn, change_hash, change, deleted_paths, None)?;
        let mut additions = Vec::<(Position<NodeId>, String, TreeProjectionKind)>::new();
        for graph_op in change.hunks() {
            match graph_op {
                GraphOp::FileAdd {
                    add_inode, path, ..
                } => additions.push((
                    Position::new(change_id, add_inode.start),
                    path.clone(),
                    TreeProjectionKind::File,
                )),
                GraphOp::DirAdd {
                    add_inode, path, ..
                } => additions.push((
                    Position::new(change_id, add_inode.start),
                    path.clone(),
                    TreeProjectionKind::Directory,
                )),
                _ => {}
            }
        }
        let addition_positions: HashSet<Position<NodeId>> =
            additions.iter().map(|(position, _, _)| *position).collect();

        // Resolve every non-add inode before allocating or mutating derived
        // indexes. Missing change IDs and reverse inode rows fail closed here.
        for op in &ops {
            let internal_change = txn
                .get_internal(&op.inode.change)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "tree projection references unknown change {}",
                        op.inode.change.to_base32()
                    ),
                })?;
            let position = Position::new(internal_change, op.inode.pos);
            if addition_positions.contains(&position) {
                continue;
            }
            txn.position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("tree projection position {} has no inode", position),
                })?;
        }

        let mut projection_ops = Vec::new();
        for (position, path, kind) in additions {
            let inode = if let Some(existing) = txn
                .position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
            {
                existing
            } else if !preserve_existing_tree_paths {
                match txn
                    .get_inode(&path)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                {
                    Some(staged)
                        if txn
                            .inode_position(staged)
                            .map_err(|error| RepositoryError::Database(error.to_string()))?
                            .is_none() =>
                    {
                        staged
                    }
                    Some(occupied) => {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "cannot add '{}': path is already bound to graph inode {}",
                                path,
                                occupied.get()
                            ),
                        });
                    }
                    None => txn
                        .alloc_inode()
                        .map_err(|error| RepositoryError::Database(error.to_string()))?,
                }
            } else {
                txn.alloc_inode()
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
            };
            projection_ops.push(TreeProjectionOperation::Add {
                inode,
                path: None,
                position: Some(position),
                kind,
            });
        }

        for graph_op in change.hunks() {
            let (inode_position, kind) = match graph_op {
                GraphOp::FileUndel { undel, .. } => (
                    external_position(change_hash, undel.inode)?,
                    TreeProjectionKind::File,
                ),
                GraphOp::DirUndel { undel, .. } => (
                    external_position(change_hash, undel.inode)?,
                    TreeProjectionKind::Directory,
                ),
                _ => continue,
            };
            let internal_change = txn
                .get_internal(&inode_position.change)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "undelete references unknown change {}",
                        inode_position.change.to_base32()
                    ),
                })?;
            let position = Position::new(internal_change, inode_position.pos);
            let inode = txn
                .position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("undelete position {} has no inode", position),
                })?;
            projection_ops.push(TreeProjectionOperation::Add {
                inode,
                path: None,
                position: Some(position),
                kind,
            });
        }

        let prerequisites =
            TreeProjectionPlan::plan(&*txn, projection_ops).map_err(projection_error)?;
        Ok(PreparedTreeProjection {
            ops,
            prerequisites,
            change_id,
            change: change.clone(),
        })
    }

    pub(super) fn apply_tree_projection(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        prepared: &PreparedTreeProjection,
        view_name: &str,
        preserve_existing_tree_paths: bool,
    ) -> Result<HashSet<String>, RepositoryError> {
        let claim_entries = super::name_resolution::path_claim_events_for_change(
            &*txn,
            prepared.change_id,
            &prepared.change,
        )?;
        for entry in claim_entries {
            txn.put_path_claim(&entry.path, &entry.event)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        }

        let (journal, changed) = self.merge_deferred_tree_ops(&prepared.ops)?;
        let view = txn
            .get_view(view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visibility = graph_visibility_closure(&*txn, &view)?;
        self.validate_deferred_tree_metadata(&*txn, &journal, &visibility)?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &*txn,
            &self.change_store,
            &view,
            &visibility,
        )?;

        let affected = if preserve_existing_tree_paths {
            HashSet::new()
        } else {
            self.apply_deferred_tree_ops_in_txn(txn, &journal, &claim_visibility)?
        };
        if changed {
            self.persist_deferred_tree_journal(&journal)?;
        }
        Ok(affected)
    }

    fn deferred_tree_journal_path(&self) -> PathBuf {
        self.dot_dir.join(DEFERRED_TREE_JOURNAL)
    }

    fn load_deferred_tree_journal(&self) -> Result<DeferredTreeJournal, RepositoryError> {
        let path = self.deferred_tree_journal_path();
        if !path.is_file() {
            return Ok(DeferredTreeJournal::default());
        }
        let journal: DeferredTreeJournal = serde_json::from_slice(&std::fs::read(path)?)?;
        if journal.version != DEFERRED_TREE_JOURNAL_VERSION {
            return Err(RepositoryError::Serialization(format!(
                "unsupported deferred TREE journal version {}",
                journal.version
            )));
        }
        Ok(journal)
    }

    /// Project path/lifecycle state from durable structural claims. TREE is a
    /// strict one-to-one cache and never participates in claimant selection.
    pub(super) fn project_tree_for_visibility<T>(
        &self,
        txn: &T,
        visibility: &GraphVisibilityClosure,
    ) -> Result<TreeProjection, RepositoryError>
    where
        T: GraphTxnT
            + TreeTxnT
            + PathClaimTxnT
            + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>,
    {
        self.project_tree_for_visibility_scoped(txn, visibility, None)
    }

    /// Path-scoped variant of [`Self::project_tree_for_visibility`].
    ///
    /// Resolving presence for a SINGLE path must not pay the absent-liveness
    /// cost of every other absent claim in the view. On a large repository the
    /// exhaustive absent check runs `retrieve_graph` per absent entry and is a
    /// measured per-entry bottleneck (14–62 s each, 659 entries per
    /// projection), even when the requested path is already present. With
    /// `wanted = Some(path)` the expensive liveness check runs only for that
    /// path; other absent claims are recorded absent without the check, which
    /// is exact for the requested path and irrelevant to the caller. Consumers
    /// that need the complete projection pass `None` and keep the full check.
    pub(super) fn project_tree_for_visibility_scoped<T>(
        &self,
        txn: &T,
        visibility: &GraphVisibilityClosure,
        wanted: Option<&str>,
    ) -> Result<TreeProjection, RepositoryError>
    where
        T: GraphTxnT
            + TreeTxnT
            + PathClaimTxnT
            + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>,
    {
        let reduced = super::name_resolution::reduce_path_claims(txn, visibility)?;
        if std::env::var("ATOMIC_DEBUG_PROJECTION").is_ok() {
            eprintln!(
                "PTV reduced present={} absent={}",
                reduced.present.len(),
                reduced.absent.len()
            );
        }
        let name_conflicts = reduced.conflicts;
        let alive_inodes: HashSet<Inode> = reduced
            .present
            .iter()
            .map(|side| side.inode)
            .chain(
                name_conflicts
                    .values()
                    .flat_map(|conflict| conflict.sides.iter().map(|side| side.inode)),
            )
            .collect();
        let alive_paths: HashSet<String> = reduced
            .present
            .iter()
            .map(|side| side.path.clone())
            .chain(name_conflicts.keys().cloned())
            .collect();
        let mut projection = TreeProjection::default();
        for side in reduced.present {
            let item = if side.is_directory() {
                OutputItem::directory_at(side.path.clone(), side.inode, side.position)
            } else {
                OutputItem::file(side.path.clone(), side.inode, side.position)
            };
            if projection.present.insert(side.path.clone(), item).is_some() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "PATH_CLAIMS projected more than one uncontested entry at '{}'",
                        side.path
                    ),
                });
            }
            projection.present_metadata.insert(side.path.clone(), side);
        }
        let debug_projection = std::env::var("ATOMIC_DEBUG_PROJECTION").is_ok();
        let mut absent_checks = 0u64;
        let mut absent_check_ms = 0u128;
        for absent in reduced.absent {
            let wanted_here = wanted.is_none_or(|path| path == absent.path.as_str());
            if wanted_here
                && !absent.directory
                && !alive_inodes.contains(&absent.inode)
                && !alive_paths.contains(&absent.path)
            {
                let check_start = std::time::Instant::now();
                let alive = self.inode_renders_alive_content(
                    txn,
                    absent.inode,
                    absent.position,
                    visibility,
                )?;
                absent_checks += 1;
                absent_check_ms += check_start.elapsed().as_millis();
                if debug_projection {
                    eprintln!(
                        "PTV absent#{} path={} inode={} ms={}",
                        absent_checks,
                        absent.path,
                        absent.inode.get(),
                        check_start.elapsed().as_millis()
                    );
                }
                if alive {
                    projection.present.insert(
                        absent.path.clone(),
                        OutputItem::file(absent.path, absent.inode, absent.position),
                    );
                    continue;
                }
            }
            if projection.present.contains_key(&absent.path)
                || name_conflicts.contains_key(&absent.path)
            {
                continue;
            }
            projection
                .absent_metadata
                .insert(absent.path.clone(), absent.clone());
            projection.absent.push(if absent.directory {
                MaterializedEntry::absent_directory(absent.path, absent.inode)
            } else {
                MaterializedEntry::absent(absent.path, Some(absent.inode))
            });
        }
        projection
            .absent
            .sort_by(|left, right| left.path().cmp(right.path()));
        projection.name_conflicts = name_conflicts;
        if debug_projection {
            eprintln!("PTV done absent_checks={absent_checks} absent_check_ms={absent_check_ms}");
        }
        Ok(projection)
    }

    #[allow(dead_code)]
    fn project_tree_for_visibility_legacy<T>(
        &self,
        txn: &T,
        visibility: &GraphVisibilityClosure,
    ) -> Result<TreeProjection, RepositoryError>
    where
        T: GraphTxnT + TreeTxnT,
    {
        let journal = self.load_deferred_tree_journal()?;
        let mut visible_hashes = HashSet::with_capacity(visibility.len());
        for change_id in visibility.iter_dependency_first().copied() {
            let hash = txn
                .get_external(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "validated visible change {} has no external hash",
                        change_id.get()
                    ))
                })?;
            visible_hashes.insert(hash);
        }
        let ordered_ops = causally_order_tree_ops(txn, &journal.ops)?;
        let desired = desired_tree_paths_for_txn(txn, &ordered_ops, &visible_hashes)?;

        let mut current = HashMap::new();
        for entry in txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let (path, inode) = entry.map_err(|e| RepositoryError::Database(e.to_string()))?;
            let Some(position) = txn
                .inode_position(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            else {
                continue;
            };
            if position.change.is_root() {
                continue;
            }
            let Some(change) = txn
                .get_external(position.change)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            else {
                continue;
            };
            current.insert(
                Position::new(change, position.pos),
                (
                    inode,
                    path,
                    txn.is_directory(inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?,
                    position,
                ),
            );
        }

        let mut positions: HashSet<Position<Hash>> = current.keys().copied().collect();
        positions.extend(desired.keys().copied());

        let mut projection = TreeProjection::default();
        let mut absent_by_path = HashMap::new();
        for external_position in positions {
            let resolved = if let Some((inode, path, is_directory, position)) =
                current.get(&external_position)
            {
                Some((*inode, Some(path.clone()), *is_directory, *position))
            } else {
                let internal_change = txn
                    .get_internal(&external_position.change)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection references unknown change {}",
                            external_position.change.to_base32()
                        ),
                    })?;
                let position = Position::new(internal_change, external_position.pos);
                let inode = txn
                    .position_inode(position)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::InvalidOperation {
                        message: format!("tree projection position {} has no inode", position),
                    })?;
                let cached_directory = txn
                    .is_directory(inode)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let is_directory = desired
                    .get(&external_position)
                    .and_then(|state| state.directory)
                    .unwrap_or(cached_directory);
                if is_directory != cached_directory {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection kind for inode {} disagrees with DIRECTORIES",
                            inode.get()
                        ),
                    });
                }
                Some((
                    inode,
                    txn.get_path(inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?,
                    is_directory,
                    position,
                ))
            };
            let Some((inode, current_path, is_directory, position)) = resolved else {
                continue;
            };
            if let Some(expected) = desired
                .get(&external_position)
                .and_then(|state| state.directory)
            {
                if expected != is_directory {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection kind for inode {} disagrees with DIRECTORIES",
                            inode.get()
                        ),
                    });
                }
            }

            if !visibility.contains(position.change) {
                continue;
            }

            match desired.get(&external_position) {
                None => {
                    if let Some(path) = current_path {
                        let item = if is_directory {
                            OutputItem::directory_at(path.clone(), inode, position)
                        } else {
                            OutputItem::file(path.clone(), inode, position)
                        };
                        projection.present.insert(path, item);
                    }
                }
                Some(state) => {
                    let mut projected_path = if state.ambiguous {
                        current_path
                            .clone()
                            .or_else(|| state.last_present_path.clone())
                    } else {
                        state.desired_path.clone()
                    };
                    if projected_path.is_none()
                        && state.deleted
                        && !is_directory
                        && crate::repository::status::is_file_alive_via_retrieval(
                            txn, inode, position, visibility,
                        )?
                    {
                        projected_path = state
                            .last_present_path
                            .clone()
                            .or_else(|| current_path.clone());
                    }

                    if let Some(path) = projected_path {
                        let item = if is_directory {
                            OutputItem::directory_at(path.clone(), inode, position)
                        } else {
                            OutputItem::file(path.clone(), inode, position)
                        };
                        projection.present.insert(path, item);
                    } else {
                        for path in &state.known_paths {
                            absent_by_path.insert(
                                path.clone(),
                                ProjectedAbsent {
                                    path: path.clone(),
                                    inode,
                                    position,
                                    directory: is_directory,
                                    deleted_by: state.maximal_changes.clone(),
                                },
                            );
                        }
                        if let Some(path) = current_path {
                            absent_by_path.insert(
                                path.clone(),
                                ProjectedAbsent {
                                    path,
                                    inode,
                                    position,
                                    directory: is_directory,
                                    deleted_by: state.maximal_changes.clone(),
                                },
                            );
                        }
                    }
                }
            }
        }

        absent_by_path.retain(|path, _| !projection.present.contains_key(path));
        projection.absent = absent_by_path
            .values()
            .map(|entry| {
                if entry.directory {
                    MaterializedEntry::absent_directory(entry.path.clone(), entry.inode)
                } else {
                    MaterializedEntry::absent(entry.path.clone(), Some(entry.inode))
                }
            })
            .collect();
        projection
            .absent
            .sort_by(|left, right| left.path().cmp(right.path()));
        projection.absent_metadata = absent_by_path;
        Ok(projection)
    }

    fn merge_deferred_tree_ops(
        &self,
        ops: &[DeferredTreeOp],
    ) -> Result<(DeferredTreeJournal, bool), RepositoryError> {
        let mut journal = self.load_deferred_tree_journal()?;
        let mut changed = false;

        // Every graph-backed lifecycle operation participates. Sparse,
        // view-order-dependent participation made TREE state depend on which
        // view happened to be active when an operation arrived.
        for op in ops {
            if let Some(existing) = journal.ops.iter_mut().find(|existing| {
                existing.change == op.change
                    && existing.inode == op.inode
                    && existing.action == op.action
            }) {
                if existing.baseline_path.is_none() {
                    existing.baseline_path.clone_from(&op.baseline_path);
                    changed = true;
                }
                if existing.directory.is_none() {
                    existing.directory = op.directory;
                    changed = true;
                }
            } else {
                journal.ops.push(op.clone());
                changed = true;
            }
        }
        Ok((journal, changed))
    }

    fn persist_deferred_tree_journal(
        &self,
        journal: &DeferredTreeJournal,
    ) -> Result<(), RepositoryError> {
        let path = self.deferred_tree_journal_path();
        let mut temp = tempfile::NamedTempFile::new_in(&self.dot_dir)?;
        serde_json::to_writer_pretty(temp.as_file_mut(), journal)?;
        temp.as_file_mut().write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| {
            RepositoryError::Io(std::io::Error::other(format!(
                "failed to persist deferred TREE journal: {}",
                error
            )))
        })?;
        self.sync_dot_dir()
    }

    fn deferred_tree_alignment_pending_path(&self) -> PathBuf {
        self.dot_dir.join(DEFERRED_TREE_ALIGNMENT_PENDING)
    }

    fn write_deferred_tree_alignment_pending(
        &self,
        source_view: &str,
        target_view: &str,
    ) -> Result<(), RepositoryError> {
        let pending = PendingTreeAlignment {
            version: DEFERRED_TREE_JOURNAL_VERSION,
            source_view: source_view.to_string(),
            target_view: target_view.to_string(),
        };
        let path = self.deferred_tree_alignment_pending_path();
        let mut temp = tempfile::NamedTempFile::new_in(&self.dot_dir)?;
        serde_json::to_writer(temp.as_file_mut(), &pending)?;
        temp.as_file_mut().write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| {
            RepositoryError::Io(std::io::Error::other(format!(
                "failed to persist deferred TREE alignment marker: {}",
                error
            )))
        })?;
        self.sync_dot_dir()?;
        Ok(())
    }

    fn load_deferred_tree_alignment_pending(
        &self,
    ) -> Result<PendingTreeAlignment, RepositoryError> {
        let pending: PendingTreeAlignment =
            serde_json::from_slice(&std::fs::read(self.deferred_tree_alignment_pending_path())?)?;
        if pending.version != DEFERRED_TREE_JOURNAL_VERSION {
            return Err(RepositoryError::Serialization(format!(
                "unsupported deferred TREE alignment version {}",
                pending.version
            )));
        }
        Ok(pending)
    }

    fn clear_deferred_tree_alignment_pending(&self) -> Result<(), RepositoryError> {
        match std::fs::remove_file(self.deferred_tree_alignment_pending_path()) {
            Ok(()) => self.sync_dot_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RepositoryError::Io(error)),
        }
    }

    pub(super) fn has_pending_deferred_tree_alignment(&self) -> bool {
        self.deferred_tree_alignment_pending_path().is_file()
    }

    fn validate_deferred_tree_metadata<T: GraphTxnT + TreeTxnT>(
        &self,
        txn: &T,
        journal: &DeferredTreeJournal,
        visibility: &GraphVisibilityClosure,
    ) -> Result<(), RepositoryError> {
        let mut visible_hashes = HashSet::with_capacity(visibility.len());
        for change_id in visibility.iter_dependency_first().copied() {
            let hash = txn
                .get_external(change_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "validated visible change {} has no external hash",
                        change_id.get()
                    ),
                })?;
            visible_hashes.insert(hash);
        }
        let ordered_ops = causally_order_tree_ops(txn, &journal.ops)?;
        let desired = desired_tree_paths_for_txn(txn, &ordered_ops, &visible_hashes)?;
        for (external_position, state) in desired {
            let internal_change = txn
                .get_internal(&external_position.change)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "tree projection references unknown change {}",
                        external_position.change.to_base32()
                    ),
                })?;
            let position = Position::new(internal_change, external_position.pos);
            let inode = txn
                .position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("tree projection position {} has no inode", position),
                })?;
            if let Some(expected_directory) = state.directory {
                let cached_directory = txn
                    .is_directory(inode)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?;
                if expected_directory != cached_directory {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection kind for inode {} disagrees with DIRECTORIES",
                            inode.get()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    pub(super) fn realign_tree_projection_in_txn(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        view_name: &str,
    ) -> Result<HashSet<String>, RepositoryError> {
        let journal = self.load_deferred_tree_journal()?;
        let view = txn
            .get_view(view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        // Legacy repositories can hold members whose dependency metadata
        // predates the index; realignment must tolerate them as leaves
        // (dev #196/#206 parity) instead of refusing every unrecord.
        let visibility = GraphVisibilityClosure::try_from_membership_lenient(
            &*txn,
            &super::filter::view_membership(txn, &view)?,
        )?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &*txn,
            &self.change_store,
            &view,
            &visibility,
        )?;
        self.apply_deferred_tree_ops_in_txn(txn, &journal, &claim_visibility)
    }

    fn apply_deferred_tree_ops_in_txn(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        _journal: &DeferredTreeJournal,
        visibility: &GraphVisibilityClosure,
    ) -> Result<HashSet<String>, RepositoryError> {
        let projection = self.project_tree_for_visibility(&*txn, visibility)?;
        let desired: HashMap<Inode, (String, bool)> = projection
            .present
            .into_values()
            .map(|item| (item.inode, (item.path, item.is_directory)))
            .collect();
        let mut operations = Vec::new();
        let mut affected = HashSet::new();

        for entry in txn
            .iter_tree()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (path, inode) =
                entry.map_err(|error| RepositoryError::Database(error.to_string()))?;
            if txn
                .inode_position(inode)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .is_none()
            {
                // A TREE row whose inode has no graph position is stale
                // (retired or orphaned). The realign owns TREE, so retire the
                // row instead of leaving an owner the projection cannot match;
                // otherwise the desired rebind below conflicts with a ghost.
                affected.insert(path);
                operations.push(TreeProjectionOperation::Delete {
                    inode,
                    retire: false,
                });
                continue;
            }
            if desired
                .get(&inode)
                .is_none_or(|(desired_path, _)| desired_path != &path)
            {
                affected.insert(path);
                operations.push(TreeProjectionOperation::Delete {
                    inode,
                    retire: false,
                });
            }
        }

        let desired_paths: HashMap<String, Inode> = desired
            .iter()
            .map(|(inode, (path, _))| (path.clone(), *inode))
            .collect();
        for (inode, (path, is_directory)) in &desired {
            let current = txn
                .get_path(*inode)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let kind = if *is_directory {
                TreeProjectionKind::Directory
            } else {
                TreeProjectionKind::File
            };
            match current.as_deref() {
                Some(current_path) if current_path == path => {}
                Some(current_path) => {
                    affected.insert(current_path.to_string());
                    affected.insert(path.clone());
                    operations.push(TreeProjectionOperation::Move {
                        inode: *inode,
                        path: path.clone(),
                    });
                }
                None => {
                    affected.insert(path.clone());
                    operations.push(TreeProjectionOperation::Undelete {
                        inode: *inode,
                        path: path.clone(),
                        kind,
                    });
                }
            }
            if *is_directory {
                let empty = !desired_paths
                    .keys()
                    .any(|candidate| projected_parent(candidate) == Some(path.as_str()));
                operations.push(TreeProjectionOperation::DirectoryOccupancy {
                    inode: *inode,
                    empty,
                });
            }
        }

        TreeProjectionPlan::plan(&*txn, operations)
            .map_err(projection_error)?
            .apply(txn)
            .map_err(projection_error)?;
        txn.validate_tree_bijection()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(affected)
    }

    #[allow(dead_code)]
    fn apply_deferred_tree_ops_in_txn_legacy(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        journal: &DeferredTreeJournal,
        visibility: &GraphVisibilityClosure,
    ) -> Result<HashSet<String>, RepositoryError> {
        let mut visible_hashes = HashSet::with_capacity(visibility.len());
        for change_id in visibility.iter_dependency_first().copied() {
            let hash = txn
                .get_external(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "validated visible change {} has no external hash",
                        change_id.get()
                    ))
                })?;
            visible_hashes.insert(hash);
        }

        let ordered_ops = causally_order_tree_ops(&*txn, &journal.ops)?;
        let desired = desired_tree_paths_for_txn(&*txn, &ordered_ops, &visible_hashes)?;
        let mut operations = Vec::new();
        let mut affected_paths = HashSet::new();
        let mut final_paths = HashMap::<String, Inode>::new();
        for entry in txn
            .iter_tree()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (path, inode) =
                entry.map_err(|error| RepositoryError::Database(error.to_string()))?;
            final_paths.insert(path, inode);
        }
        let mut projected_directories = Vec::<(String, Inode)>::new();
        let mut path_updates = Vec::<(Inode, Option<String>, Option<String>)>::new();

        for (external_position, state) in desired {
            let internal_change = txn
                .get_internal(&external_position.change)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "tree projection references unknown change {}",
                        external_position.change.to_base32()
                    ),
                })?;
            let position = Position::new(internal_change, external_position.pos);
            let inode = txn
                .position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("tree projection position {} has no inode", position),
                })?;
            let cached_directory = txn
                .is_directory(inode)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let is_directory = state.directory.unwrap_or(cached_directory);
            if is_directory != cached_directory {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "tree projection kind for inode {} disagrees with DIRECTORIES",
                        inode.get()
                    ),
                });
            }

            let current_path = txn
                .get_path(inode)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let mut desired_path = if !visibility.contains(internal_change) {
                None
            } else if state.ambiguous {
                current_path
                    .clone()
                    .or_else(|| state.last_present_path.clone())
            } else {
                state.desired_path.clone()
            };
            if desired_path.is_none()
                && state.deleted
                && !is_directory
                && visibility.contains(internal_change)
                && crate::repository::status::is_file_alive_via_retrieval(
                    &*txn, inode, position, visibility,
                )?
            {
                desired_path = state
                    .last_present_path
                    .clone()
                    .or_else(|| current_path.clone());
            }

            path_updates.push((inode, current_path.clone(), desired_path.clone()));
            if current_path != desired_path {
                if let Some(path) = &current_path {
                    affected_paths.insert(path.clone());
                }
                if let Some(path) = &desired_path {
                    affected_paths.insert(path.clone());
                }
            }
            match desired_path.clone() {
                Some(path) if current_path.as_deref() == Some(path.as_str()) => {
                    operations.push(TreeProjectionOperation::Undelete {
                        inode,
                        path,
                        kind: if is_directory {
                            TreeProjectionKind::Directory
                        } else {
                            TreeProjectionKind::File
                        },
                    });
                }
                Some(path) if current_path.is_some() => {
                    operations.push(TreeProjectionOperation::Move { inode, path });
                }
                Some(path) => operations.push(TreeProjectionOperation::Undelete {
                    inode,
                    path,
                    kind: if is_directory {
                        TreeProjectionKind::Directory
                    } else {
                        TreeProjectionKind::File
                    },
                }),
                None if current_path.is_some() => {
                    operations.push(TreeProjectionOperation::Delete {
                        inode,
                        retire: false,
                    })
                }
                None => {}
            }
            if is_directory {
                if let Some(path) = desired_path {
                    projected_directories.push((path, inode));
                }
            }
        }

        for (inode, current_path, _) in &path_updates {
            if let Some(path) = current_path {
                if final_paths.get(path) == Some(inode) {
                    final_paths.remove(path);
                }
            }
        }
        for (inode, _, desired_path) in &path_updates {
            if let Some(path) = desired_path {
                final_paths.insert(path.clone(), *inode);
            }
        }

        // DIR_EMPTY is a projection of exact direct-child membership after all
        // visible add/delete/move/undelete effects, never of path prefixes or
        // the previous DIR_EMPTY value.
        for (path, inode) in projected_directories {
            let empty = !final_paths
                .keys()
                .any(|candidate| projected_parent(candidate) == Some(path.as_str()));
            operations.push(TreeProjectionOperation::DirectoryOccupancy { inode, empty });
        }

        TreeProjectionPlan::plan(&*txn, operations)
            .map_err(projection_error)?
            .apply(txn)
            .map_err(projection_error)?;
        Ok(affected_paths)
    }

    pub(super) fn recover_pending_deferred_tree_alignment_locked(
        &mut self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
    ) -> Result<(), RepositoryError> {
        if self.is_sandbox || !self.has_pending_deferred_tree_alignment() {
            return Ok(());
        }

        let write = operation_lock.begin_write_immediate()?;
        let mut txn = write.try_lock_deferred_tree()?;
        if !self.has_pending_deferred_tree_alignment() {
            return Ok(());
        }
        let pending = self.load_deferred_tree_alignment_pending()?;
        let journal = self.load_deferred_tree_journal()?;
        let source_view = txn
            .get_view(&pending.source_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: pending.source_view.clone(),
            })?;
        let full_visibility = graph_visibility_closure(&*txn, &source_view)?;
        let visibility = super::name_resolution::path_claim_visibility_for_view(
            &*txn,
            &self.change_store,
            &source_view,
            &full_visibility,
        )?;
        self.apply_deferred_tree_ops_in_txn(&mut txn, &journal, &visibility)?;
        self.write_current_view(&pending.source_view)?;
        txn.commit()?;
        self.current_view = pending.source_view;
        self.clear_deferred_tree_alignment_pending()?;
        Ok(())
    }

    /// Apply an exact working-copy state while preserving the CB-1B lock order.
    ///
    /// For the canonical working copy this updates the record, deferred TREE
    /// projection, and compatibility pointer under one immediate pristine write
    /// followed by the final deferred-resource lock. Linked worktrees keep their
    /// compatibility pointer local and do not rewrite the common TREE projection.
    pub(super) fn apply_working_copy_state_locked(
        &mut self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
        state: &WorkingCopyStateRef,
    ) -> Result<(), RepositoryError> {
        if operation_lock.working_copy() != state.id {
            return Err(RepositoryError::WorkingCopyIdentityMismatch {
                requested: state.id,
                actual: operation_lock.working_copy(),
            });
        }

        let record = WorkingCopyRecord {
            id: state.id,
            location_fingerprint: state.location_fingerprint,
            desired_view: state.desired_view,
            desired_state: state.desired_state,
            materialized_state: state.materialized_state,
            materialized_manifest: state.materialized_manifest,
        };

        if self.working_copy_dot_dir() != self.dot_dir {
            let mut txn = operation_lock.begin_write_immediate()?;
            let current = txn
                .get_working_copy(state.id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: state.id })?;
            if current.location_fingerprint != state.location_fingerprint {
                return Err(RepositoryError::WorkingCopyLocationMismatch { id: state.id });
            }
            let target_view = ViewTxnT::get_view_by_id(&*txn, state.desired_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidRepository {
                    reason: format!(
                        "working-copy state {} references missing desired view {}",
                        state.id, state.desired_view
                    ),
                })?;
            let target_name = target_view.name.clone();
            txn.put_working_copy(&record)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            txn.commit()?;
            self.write_current_view(&target_name)?;
            self.current_view = target_name;
            return Ok(());
        }

        let mut write = operation_lock.begin_write_immediate()?;
        let current = write
            .get_working_copy(state.id)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: state.id })?;
        if current.location_fingerprint != state.location_fingerprint {
            return Err(RepositoryError::WorkingCopyLocationMismatch { id: state.id });
        }
        let target_view = ViewTxnT::get_view_by_id(&*write, state.desired_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::InvalidRepository {
                reason: format!(
                    "working-copy state {} references missing desired view {}",
                    state.id, state.desired_view
                ),
            })?;
        let target_name = target_view.name.clone();
        let persisted_view = Self::read_current_view(&self.dot_dir)?;
        if current.desired_view == state.desired_view
            && persisted_view == target_name
            && !self.has_pending_deferred_tree_alignment()
        {
            write
                .put_working_copy(&record)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            write.commit()?;
            self.current_view = target_name;
            return Ok(());
        }

        let mut txn = write.try_lock_deferred_tree()?;
        let full_visibility = graph_visibility_closure(&*txn, &target_view)?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &*txn,
            &self.change_store,
            &target_view,
            &full_visibility,
        )?;
        let journal = self.load_deferred_tree_journal()?;
        let old_view = persisted_view;
        self.write_deferred_tree_alignment_pending(&old_view, &target_name)?;
        if let Err(error) =
            self.apply_deferred_tree_ops_in_txn(&mut txn, &journal, &claim_visibility)
        {
            let _ = self.clear_deferred_tree_alignment_pending();
            return Err(error);
        }
        txn.put_working_copy(&record)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        if let Err(error) = self.write_current_view(&target_name) {
            let _ = self.clear_deferred_tree_alignment_pending();
            return Err(error);
        }
        if let Err(error) = txn.commit() {
            if self.write_current_view(&old_view).is_ok() {
                let _ = self.clear_deferred_tree_alignment_pending();
            }
            return Err(error);
        }
        self.current_view = target_name;
        self.clear_deferred_tree_alignment_pending()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::ChangePosition;

    fn hash(label: &str) -> Hash {
        Hash::of(label.as_bytes())
    }

    fn set(
        change: Hash,
        inode: Position<Hash>,
        baseline: Option<&str>,
        path: &str,
    ) -> DeferredTreeOp {
        DeferredTreeOp {
            change,
            inode,
            baseline_path: baseline.map(str::to_string),
            directory: Some(false),
            action: DeferredTreeAction::Set {
                path: path.to_string(),
            },
        }
    }

    #[test]
    fn planner_handles_causal_rename_chains_and_view_visibility() {
        let inode = Position::new(hash("creator"), ChangePosition::new(7));
        let first = hash("move-a-b");
        let second = hash("move-b-c");
        let ops = vec![
            set(first, inode, Some("a.txt"), "b.txt"),
            set(second, inode, Some("stale-baseline.txt"), "c.txt"),
        ];

        let base = desired_tree_paths(&ops, &HashSet::new(), |_, _| Ok(false)).unwrap();
        assert_eq!(base[&inode].desired_path.as_deref(), Some("a.txt"));

        let visible = HashSet::from([first, second]);
        let moved = desired_tree_paths(&ops, &visible, |descendant, ancestor| {
            Ok(descendant == second && ancestor == first)
        })
        .unwrap();
        assert_eq!(moved[&inode].desired_path.as_deref(), Some("c.txt"));
    }

    #[test]
    fn planner_applies_causally_later_visible_deletion() {
        let inode = Position::new(hash("creator"), ChangePosition::new(9));
        let moved = hash("move");
        let deleted = hash("delete");
        let ops = vec![
            set(moved, inode, Some("old.txt"), "new.txt"),
            DeferredTreeOp {
                change: deleted,
                inode,
                baseline_path: Some("new.txt".into()),
                directory: Some(false),
                action: DeferredTreeAction::Delete,
            },
        ];

        let desired = desired_tree_paths(
            &ops,
            &HashSet::from([moved, deleted]),
            |descendant, ancestor| Ok(descendant == deleted && ancestor == moved),
        )
        .unwrap();
        assert_eq!(desired[&inode].desired_path, None);
    }

    #[test]
    fn planner_projects_causally_later_undelete() {
        let inode = Position::new(hash("creator"), ChangePosition::new(10));
        let deleted = hash("delete");
        let restored = hash("undelete");
        let ops = vec![
            DeferredTreeOp {
                change: deleted,
                inode,
                baseline_path: Some("file.txt".into()),
                directory: Some(false),
                action: DeferredTreeAction::Delete,
            },
            set(restored, inode, None, "file.txt"),
        ];

        let desired = desired_tree_paths(
            &ops,
            &HashSet::from([deleted, restored]),
            |descendant, ancestor| Ok(descendant == restored && ancestor == deleted),
        )
        .unwrap();
        assert_eq!(desired[&inode].desired_path.as_deref(), Some("file.txt"));
        assert!(!desired[&inode].deleted);
    }

    #[test]
    fn planner_does_not_use_journal_order_for_concurrent_renames() {
        let inode = Position::new(hash("creator"), ChangePosition::new(11));
        let left = hash("left-rename");
        let right = hash("right-rename");
        let ops = vec![
            set(left, inode, Some("base.txt"), "left.txt"),
            set(right, inode, Some("base.txt"), "right.txt"),
        ];

        let desired =
            desired_tree_paths(&ops, &HashSet::from([left, right]), |_, _| Ok(false)).unwrap();
        assert!(desired[&inode].ambiguous);
        assert_eq!(desired[&inode].desired_path, None);
        assert_eq!(
            desired[&inode].last_present_path.as_deref(),
            Some("base.txt")
        );
    }

    #[test]
    fn planner_preserves_concurrent_same_path_claims() {
        let left_inode = Position::new(hash("left-creator"), ChangePosition::new(3));
        let right_inode = Position::new(hash("right-creator"), ChangePosition::new(5));
        let left = hash("left-add");
        let right = hash("right-add");
        let ops = vec![
            set(left, left_inode, None, "same.txt"),
            set(right, right_inode, None, "same.txt"),
        ];

        let desired =
            desired_tree_paths(&ops, &HashSet::from([left, right]), |_, _| Ok(false)).unwrap();
        assert_eq!(
            desired[&left_inode].desired_path.as_deref(),
            Some("same.txt")
        );
        assert_eq!(
            desired[&right_inode].desired_path.as_deref(),
            Some("same.txt")
        );
    }
}
