use super::*;

use crate::apply::{
    filter_missing_in_view, get_missing_changes, get_view_changes as get_view_changes_fn,
    write_change_to_graph, CrossViewInsertOptions, CrossViewInsertOutcome, InsertOptions,
    InsertOutcome, InsertStats,
};
use crate::repository::deferred_tree::collect_tree_ops;
use atomic_core::change::Insertion;
use atomic_core::pristine::InodeGraphOps;
use atomic_core::types::{ChangePosition, EdgeFlags, GraphNode, SerializedGraphEdge};
use std::collections::{HashMap, HashSet};

/// Check whether a file's creating change exists ONLY on the given view
/// (and no other view).  Returns `true` when it is safe to remove the
/// file's TREE / INODES entries because no other view needs them.
///
/// When the inode has no INODES position (not yet recorded) the function
/// returns `true` — there is nothing to protect.
///
/// # Complexity
///
/// O(S × C) in the worst case, where S is the number of views and C is the
/// number of visible changes per view. Path deletion is uncommon, and using
/// the canonical inherited-view filter is required for correctness on drafts.
fn is_file_only_on_view<T: GraphTxnT + ViewTxnT + TreeTxnT>(
    txn: &T,
    inode: Inode,
    current_view: &str,
) -> bool {
    // Look up the position for this inode.  If there is no position the
    // file was never recorded, so removing from TREE is safe.
    let position = match txn.inode_position(inode) {
        Ok(Some(pos)) => pos,
        _ => return true,
    };

    let creating_change = position.change;
    if creating_change.is_root() {
        return true;
    }

    // Walk every view and check whether the creating change appears on
    // any view OTHER than `current_view`.
    let view_names = match txn.list_views() {
        Ok(names) => names,
        Err(_) => return true,
    };

    for name in view_names {
        if name == current_view {
            continue;
        }
        let view = match txn.get_view(&name) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        if collect_visible_change_ids(txn, &view)
            .map(|ids| ids.contains(&creating_change))
            .unwrap_or(false)
        {
            // Another view still references this file — not safe to remove.
            return false;
        }
    }

    // No other view references the creating change.
    true
}

/// Timing details for the git-import fresh-write path.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImportWriteTimings {
    pub assemble_ms: u128,
    pub save_ms: u128,
    pub apply_ms: u128,
    pub commit_ms: u128,
    pub direct_graph_ms: u128,
    pub direct_crdt_ms: u128,
}

/// Outcome from writing an already-recorded git-import commit.
#[derive(Debug, Clone)]
pub struct ImportWriteOutcome {
    pub hash: Hash,
    pub timings: ImportWriteTimings,
    pub insert: InsertOutcome,
}

/// Existing graph line metadata used by git import to lazily rebuild its
/// in-memory line index after a prior fallback or interrupted fast path.
#[derive(Debug, Clone)]
pub struct ImportLineIndexSeed {
    pub inode_pos: Position<Hash>,
    pub lines: Vec<ImportLineIndexSeedLine>,
}

#[derive(Debug, Clone)]
pub struct ImportLineIndexSeedLine {
    pub change: Hash,
    pub start: ChangePosition,
    pub end: ChangePosition,
    pub incoming_by: Hash,
}

#[derive(Default)]
struct ImportGraphFirstVertexCache {
    by_inode: HashMap<Inode, ImportGraphFirstInodeCache>,
    by_end_pos: HashMap<(Inode, Position<NodeId>), GraphNode<NodeId>>,
    by_start_pos: HashMap<(Inode, Position<NodeId>), GraphNode<NodeId>>,
}

#[derive(Default)]
struct ImportGraphFirstInodeCache {
    by_end: HashMap<Position<NodeId>, GraphNode<NodeId>>,
    by_start: HashMap<Position<NodeId>, GraphNode<NodeId>>,
}

impl ImportGraphFirstVertexCache {
    fn find_end<T>(
        &mut self,
        txn: &T,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, RepositoryError>
    where
        T: GraphTxnT + InodeGraphOps,
    {
        if let Some(node) = self.by_end_pos.get(&(inode, pos)) {
            return Ok(Some(*node));
        }
        let Some(node) = txn
            .find_block_end_in_inode(inode, pos)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(None);
        };
        self.by_end_pos.insert((inode, pos), node);
        Ok(Some(node))
    }

    fn find_start<T>(
        &mut self,
        txn: &T,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, RepositoryError>
    where
        T: GraphTxnT + InodeGraphOps,
    {
        if let Some(node) = self.by_start_pos.get(&(inode, pos)) {
            return Ok(Some(*node));
        }
        let Some(node) = txn
            .find_block_in_inode(inode, pos)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(None);
        };
        self.by_start_pos.insert((inode, pos), node);
        Ok(Some(node))
    }

    fn load<T>(
        &mut self,
        txn: &T,
        inode: Inode,
    ) -> Result<&ImportGraphFirstInodeCache, RepositoryError>
    where
        T: TreeTxnT,
    {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.by_inode.entry(inode) {
            let mut inode_cache = ImportGraphFirstInodeCache::default();
            let vertices = txn
                .iter_inode_vertices(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            for result in vertices {
                let (node, _edge) = result.map_err(|e| RepositoryError::Database(e.to_string()))?;
                inode_cache.by_end.entry(node.end_pos()).or_insert(node);
                inode_cache
                    .by_start
                    .entry(node.start_pos())
                    .and_modify(|existing| {
                        if existing.start == existing.end && node.start != node.end {
                            *existing = node;
                        }
                    })
                    .or_insert(node);
            }
            entry.insert(inode_cache);
        }

        self.by_inode.get(&inode).ok_or_else(|| {
            RepositoryError::Apply(format!(
                "missing import vertex cache for inode {}",
                inode.get()
            ))
        })
    }
}

type PendingImportEdge = (
    Option<Inode>,
    EdgeFlags,
    GraphNode<NodeId>,
    GraphNode<NodeId>,
);

fn import_direct_source(
    pos: &Position<Option<Hash>>,
    by_end: &HashMap<ChangePosition, GraphNode<NodeId>>,
) -> Option<GraphNode<NodeId>> {
    match pos.change {
        Some(hash) if hash == Hash::NONE => Some(GraphNode::root()),
        None => by_end.get(&pos.pos).copied(),
        _ => None,
    }
}

fn import_direct_inode(
    pos: &Position<Option<Hash>>,
    inode_by_pos: &HashMap<ChangePosition, Inode>,
) -> Option<Inode> {
    match pos.change {
        None => inode_by_pos.get(&pos.pos).copied(),
        _ => None,
    }
}

fn import_direct_can_apply(change: &Change) -> bool {
    if change.hunks().is_empty() {
        return true;
    }

    change.hunks().iter().all(|op| match op {
        GraphOp::FileAdd {
            add_name,
            add_inode,
            contents,
            ..
        } => {
            add_name.successors.is_empty()
                && add_inode.successors.is_empty()
                && contents
                    .as_ref()
                    .map(|c| c.successors.is_empty())
                    .unwrap_or(true)
        }
        GraphOp::Edit {
            change: atomic_core::change::Atom::Insertion(insertion),
            ..
        } => {
            insertion.successors.is_empty()
                && insertion.predecessors.len() == 1
                && insertion.predecessors[0].change.is_none()
                && insertion.inode.change.is_none()
        }
        _ => false,
    })
}

fn import_seed_edge_visible(edge: &SerializedGraphEdge, visible: &HashSet<NodeId>) -> bool {
    let change = edge.introduced_by();
    change.is_root() || visible.contains(&change)
}

fn import_seed_node_visible(node: GraphNode<NodeId>, visible: &HashSet<NodeId>) -> bool {
    node.change.is_root() || visible.contains(&node.change)
}

fn import_seed_is_dead<T>(
    txn: &T,
    inode: Inode,
    node: GraphNode<NodeId>,
    visible: &HashSet<NodeId>,
) -> bool
where
    T: GraphTxnT + InodeGraphOps,
{
    let mut parents = match txn.init_inode_adj(inode, node, EdgeFlags::PARENT, EdgeFlags::all()) {
        Ok(adj) => adj,
        Err(_) => return false,
    };
    while let Some(edge) = txn.next_inode_adj(&mut parents) {
        let Ok(edge) = edge else {
            continue;
        };
        let flags = edge.flag();
        if flags.contains(EdgeFlags::PARENT)
            && flags.contains(EdgeFlags::DELETED)
            && import_seed_edge_visible(&edge, visible)
        {
            return true;
        }
    }
    false
}

fn import_seed_alive_reaches<T>(
    txn: &T,
    inode: Inode,
    start: GraphNode<NodeId>,
    target: GraphNode<NodeId>,
    visible: &HashSet<NodeId>,
) -> bool
where
    T: GraphTxnT + InodeGraphOps,
{
    if start == target {
        return true;
    }

    let mut stack = vec![start];
    let mut seen = HashSet::new();

    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }

        let mut adj = match txn.init_inode_adj(inode, current, EdgeFlags::BLOCK, EdgeFlags::all()) {
            Ok(adj) => adj,
            Err(_) => continue,
        };

        while let Some(edge) = txn.next_inode_adj(&mut adj) {
            let Ok(edge) = edge else {
                continue;
            };
            let flags = edge.flag();
            if flags.contains(EdgeFlags::PARENT)
                || flags.contains(EdgeFlags::DELETED)
                || flags.contains(EdgeFlags::PSEUDO)
                || !import_seed_edge_visible(&edge, visible)
            {
                continue;
            }

            let Some(dest) = txn
                .find_block_in_inode(inode, edge.dest())
                .ok()
                .flatten()
                .or_else(|| txn.find_block(edge.dest()).ok())
            else {
                continue;
            };
            if !import_seed_node_visible(dest, visible)
                || import_seed_is_dead(txn, inode, dest, visible)
            {
                continue;
            }
            if dest == target {
                return true;
            }
            stack.push(dest);
        }
    }

    false
}

fn import_graph_first_can_apply(change: &Change) -> bool {
    !change.hunks().is_empty()
        && change.hunks().iter().all(|op| match op {
            GraphOp::FileAdd { .. } => true,
            GraphOp::FileMove { add, .. } => {
                !add.predecessors.is_empty() && add.predecessors.len() == 1
            }
            GraphOp::Replacement { replacement, .. } => {
                !replacement.predecessors.is_empty() && replacement.predecessors.len() == 1
            }
            GraphOp::Edit {
                change: atomic_core::change::Atom::Insertion(insertion),
                ..
            } => !insertion.predecessors.is_empty() && insertion.predecessors.len() == 1,
            GraphOp::Edit {
                change: atomic_core::change::Atom::EdgeUpdate(_),
                ..
            } => true,
            _ => false,
        })
}

fn import_graph_first_position<T: GraphTxnT>(
    txn: &T,
    pos: &Position<Option<Hash>>,
    change_id: NodeId,
) -> Result<Position<NodeId>, RepositoryError> {
    let resolved_change = match pos.change {
        Some(hash) if hash == Hash::NONE => NodeId::ROOT,
        Some(hash) => txn
            .get_internal(&hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::Apply(format!("missing dependency {}", hash)))?,
        None => change_id,
    };

    Ok(Position::new(resolved_change, pos.pos))
}

fn import_graph_first_node<T: GraphTxnT>(
    txn: &T,
    node: GraphNode<Option<Hash>>,
    change_id: NodeId,
) -> Result<GraphNode<NodeId>, RepositoryError> {
    let pos = Position::new(node.change, node.start);
    let resolved = import_graph_first_position(txn, &pos, change_id)?;
    Ok(GraphNode {
        change: resolved.change,
        start: node.start,
        end: node.end,
    })
}

fn import_graph_first_resolved_inode<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    inode_pos: &Position<Option<Hash>>,
    change_id: NodeId,
) -> Result<Option<Inode>, RepositoryError> {
    let resolved = import_graph_first_position(txn, inode_pos, change_id)?;
    if resolved.change.is_root() {
        return Ok(None);
    }
    txn.position_inode(resolved)
        .map_err(|e| RepositoryError::Database(e.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn import_graph_first_source<T>(
    txn: &T,
    pos: &Position<Option<Hash>>,
    inode_pos: &Position<Option<Hash>>,
    resolved_inode: Option<Inode>,
    old_by_end: &HashMap<Position<NodeId>, GraphNode<NodeId>>,
    current_by_end: &HashMap<Position<NodeId>, GraphNode<NodeId>>,
    vertex_cache: &mut ImportGraphFirstVertexCache,
    change_id: NodeId,
) -> Result<GraphNode<NodeId>, RepositoryError>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let resolved = import_graph_first_position(txn, pos, change_id)?;

    if resolved.change == change_id {
        if let Some(node) = current_by_end.get(&resolved) {
            return Ok(*node);
        }
    }

    if let Some(node) = old_by_end.get(&resolved) {
        return Ok(*node);
    }

    let resolved_inode_pos = import_graph_first_position(txn, inode_pos, change_id)?;
    if resolved == resolved_inode_pos {
        return Ok(GraphNode {
            change: resolved.change,
            start: resolved.pos,
            end: resolved.pos,
        });
    }

    if let Some(inode) = resolved_inode {
        if let Some(node) = vertex_cache.find_end(txn, inode, resolved)? {
            return Ok(node);
        }
        if let Some(node) = vertex_cache.load(txn, inode)?.by_end.get(&resolved) {
            return Ok(*node);
        }
    }

    txn.find_block_end(resolved)
        .map_err(|e| RepositoryError::Apply(e.to_string()))
}

fn import_graph_first_successor<T>(
    txn: &T,
    pos: &Position<Option<Hash>>,
    resolved_inode: Option<Inode>,
    current_by_start: &HashMap<Position<NodeId>, GraphNode<NodeId>>,
    vertex_cache: &mut ImportGraphFirstVertexCache,
    change_id: NodeId,
) -> Result<GraphNode<NodeId>, RepositoryError>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let resolved = import_graph_first_position(txn, pos, change_id)?;
    if resolved.change == change_id {
        if let Some(node) = current_by_start.get(&resolved) {
            return Ok(*node);
        }
    }

    if let Some(inode) = resolved_inode {
        if let Some(node) = vertex_cache.find_start(txn, inode, resolved)? {
            return Ok(node);
        }
        if let Some(node) = vertex_cache.load(txn, inode)?.by_start.get(&resolved) {
            return Ok(*node);
        }
    }

    txn.find_block(resolved)
        .map_err(|e| RepositoryError::Apply(e.to_string()))
}

fn import_direct_write_insertion(
    batch: &mut atomic_core::apply::CachedWriteGraphTxn<'_, '_>,
    change_id: NodeId,
    insertion: &Insertion<Option<Hash>>,
    by_end: &mut HashMap<ChangePosition, GraphNode<NodeId>>,
    inode_by_pos: &HashMap<ChangePosition, Inode>,
    inode_sources: &mut HashSet<(u64, GraphNode<NodeId>)>,
    inode_terminal_candidates: &mut Vec<(Inode, GraphNode<NodeId>, SerializedGraphEdge)>,
) -> Result<(), RepositoryError> {
    if !insertion.successors.is_empty() || insertion.predecessors.len() != 1 {
        return Err(RepositoryError::Apply(
            "direct import insertion requires one predecessor and no successors".to_string(),
        ));
    }

    let source = import_direct_source(&insertion.predecessors[0], by_end).ok_or_else(|| {
        RepositoryError::Apply(format!(
            "direct import missing predecessor at {:?}",
            insertion.predecessors[0]
        ))
    })?;
    let dest = GraphNode {
        change: change_id,
        start: insertion.start,
        end: insertion.end,
    };
    let inode = import_direct_inode(&insertion.inode, inode_by_pos);
    let flag = insertion.flag | EdgeFlags::BLOCK;

    batch
        .add_edge_with_reverse_inode_forward_only(inode, flag, source, dest, change_id)
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
    if let Some(inode_val) = inode {
        inode_sources.insert((inode_val.get(), source));
        let reverse_edge =
            SerializedGraphEdge::new(flag | EdgeFlags::PARENT, source.end_pos(), change_id);
        inode_terminal_candidates.push((inode_val, dest, reverse_edge));
    }
    by_end.insert(dest.end, dest);
    Ok(())
}

impl Repository {
    // Change Insertion Methods

    /// Rebuild ordered line vertex metadata for a tracked file from the
    /// current view's graph. Git import uses this as a conservative repair
    /// path when its in-memory line index is missing for a modified file.
    pub fn import_line_index_seed(
        &self,
        path: &str,
    ) -> Result<Option<ImportLineIndexSeed>, RepositoryError> {
        let normalized = path.replace('\\', "/");
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;
        let visible = collect_visible_change_ids_with_deps(&txn, &view)?;

        let Some(inode) = txn
            .get_inode(&normalized)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(position) = txn
            .inode_position(inode)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(None);
        };
        if !import_seed_node_visible(position.inode_node(), &visible) {
            return Ok(None);
        }
        let Some(inode_change) = txn
            .get_external(position.change)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(None);
        };

        let mut current = position.inode_node();
        let mut visited = HashSet::new();
        let mut lines = Vec::new();

        loop {
            if !visited.insert(current) {
                return Ok(None);
            }

            let mut adj = txn
                .init_inode_adj(
                    inode,
                    current,
                    EdgeFlags::BLOCK,
                    EdgeFlags::BLOCK | EdgeFlags::FOLDER,
                )
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            let mut alive_candidates: Vec<(GraphNode<NodeId>, NodeId)> = Vec::new();
            let mut next_dead: Option<(GraphNode<NodeId>, NodeId)> = None;

            while let Some(edge) = txn.next_inode_adj(&mut adj) {
                let edge = edge.map_err(|e| RepositoryError::Database(e.to_string()))?;
                let flags = edge.flag();
                if flags.contains(EdgeFlags::PARENT)
                    || flags.contains(EdgeFlags::DELETED)
                    || flags.contains(EdgeFlags::PSEUDO)
                    || !import_seed_edge_visible(&edge, &visible)
                {
                    continue;
                }

                let Some(dest) = txn
                    .find_block_in_inode(inode, edge.dest())
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .or_else(|| txn.find_block(edge.dest()).ok())
                else {
                    continue;
                };
                if visited.contains(&dest) || !import_seed_node_visible(dest, &visible) {
                    continue;
                }
                let introduced_by = edge.introduced_by();
                if !import_seed_is_dead(&txn, inode, dest, &visible) {
                    alive_candidates.push((dest, introduced_by));
                } else if next_dead.is_none() {
                    next_dead = Some((dest, introduced_by));
                }
            }

            let next_alive = if alive_candidates.len() <= 1 {
                alive_candidates.into_iter().next()
            } else {
                alive_candidates
                    .iter()
                    .copied()
                    .find(|(candidate, _)| {
                        let reaches_other = alive_candidates.iter().copied().any(|(other, _)| {
                            other != *candidate
                                && import_seed_alive_reaches(
                                    &txn, inode, *candidate, other, &visible,
                                )
                        });
                        let reached_by_other =
                            alive_candidates.iter().copied().any(|(other, _)| {
                                other != *candidate
                                    && import_seed_alive_reaches(
                                        &txn, inode, other, *candidate, &visible,
                                    )
                            });
                        reaches_other && !reached_by_other
                    })
                    .or_else(|| {
                        alive_candidates.iter().copied().find(|(candidate, _)| {
                            alive_candidates.iter().copied().any(|(other, _)| {
                                other != *candidate
                                    && import_seed_alive_reaches(
                                        &txn, inode, *candidate, other, &visible,
                                    )
                            })
                        })
                    })
                    .or_else(|| alive_candidates.into_iter().next())
            };

            let Some((dest, introduced_by)) = next_alive.or(next_dead) else {
                break;
            };

            let is_inode_marker = dest.start == dest.end && dest.start == position.pos;
            let is_alive = !import_seed_is_dead(&txn, inode, dest, &visible);
            if is_alive && !is_inode_marker && !dest.change.is_root() && dest.start != dest.end {
                let Some(change) = txn
                    .get_external(dest.change)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                else {
                    return Ok(None);
                };
                let Some(incoming_by) = txn
                    .get_external(introduced_by)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                else {
                    return Ok(None);
                };
                lines.push(ImportLineIndexSeedLine {
                    change,
                    start: dest.start,
                    end: dest.end,
                    incoming_by,
                });
            }

            current = dest;
        }

        Ok(Some(ImportLineIndexSeed {
            inode_pos: Position::new(inode_change, position.pos),
            lines,
        }))
    }

    /// Assemble, save, and apply a freshly imported Git commit without going
    /// through the normal `insert_change()` load/check path.
    ///
    /// This preserves the normal graph writer and CRDT table application, but
    /// avoids reloading the just-saved change and avoids the `has_change_in_graph`
    /// probe. The write transaction is opened before assembly, so globalization
    /// and application share one consistent transaction view. When
    /// `preserve_existing_tree_paths` is set, deletions and renames remain
    /// graph-only so importing into a foreign view cannot rewrite the active
    /// view's global TREE mappings.
    pub fn write_import_recorded(
        &self,
        header: ChangeHeader,
        recorded_files: &[atomic_core::record::workflow::RecordedFile],
        unhashed: serde_json::Value,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
        options: InsertOptions,
    ) -> Result<ImportWriteOutcome, RepositoryError> {
        use atomic_core::record::workflow::assemble_change;
        use atomic_core::record::workflow::assembly::AssemblyOptions;

        let mut timings = ImportWriteTimings::default();
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let assemble_start = std::time::Instant::now();
        let mut change = if recorded_files.is_empty() {
            Change::empty(header)
        } else {
            match assemble_change(
                &txn,
                recorded_files,
                header.clone(),
                &AssemblyOptions::default(),
            ) {
                Ok(result) => result.into_change(),
                Err(e) => {
                    let err_msg = e.to_string();
                    if err_msg.contains("empty") || err_msg.contains("AllEmpty") {
                        Change::empty(header)
                    } else {
                        return Err(RepositoryError::Apply(e.to_string()));
                    }
                }
            }
        };
        timings.assemble_ms = assemble_start.elapsed().as_millis();

        change.unhashed = Some(unhashed);

        let mut v3_bytes = Vec::new();
        let hash = change
            .serialize(&mut v3_bytes)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let (final_change, verified_hash) = Change::deserialize(&mut v3_bytes.as_slice())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        debug_assert_eq!(hash, verified_hash);

        let save_start = std::time::Instant::now();
        self.save_change_bytes(&hash, &v3_bytes, &final_change)?;
        timings.save_ms = save_start.elapsed().as_millis();

        let change_id = txn
            .register_change(&hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.put_change_deps(change_id, final_change.dependencies())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let tree_ops = collect_tree_ops(&txn, hash, &final_change, deleted_paths)?;

        for graph_op in final_change.hunks() {
            match graph_op {
                GraphOp::FileAdd {
                    add_inode, path, ..
                } => {
                    let new_inode = txn
                        .alloc_inode()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    let inode_position = Position::new(change_id, add_inode.start);
                    if !preserve_existing_tree_paths {
                        txn.put_tree(path, new_inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    txn.put_inode(new_inode, inode_position)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
                GraphOp::DirAdd {
                    add_inode, path, ..
                } => {
                    use atomic_core::pristine::directory_flags;

                    let new_inode = txn
                        .alloc_inode()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    let inode_position = Position::new(change_id, add_inode.start);
                    if !preserve_existing_tree_paths {
                        txn.put_tree(path, new_inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    txn.put_inode(new_inode, inode_position)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    txn.put_directory(new_inode, directory_flags::explicit_empty())
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
                GraphOp::FileDel { path, .. } if !preserve_existing_tree_paths => {
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        let dominated = is_file_only_on_view(&txn, inode, view_name);
                        if dominated {
                            let _ = txn.del_tree(path);
                            let _ = txn.del_inode(inode);
                        }
                    }
                }
                GraphOp::DirDel { path, .. } if !preserve_existing_tree_paths => {
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        let dominated = is_file_only_on_view(&txn, inode, view_name);
                        if dominated {
                            let _ = txn.del_tree(path);
                            let _ = txn.del_inode(inode);
                            let _ = txn.del_directory(inode);
                        }
                    }
                }
                GraphOp::FileMove { add, path, .. } if !preserve_existing_tree_paths => {
                    let inode_change_id = match &add.inode.change {
                        None => change_id,
                        Some(h) if *h == Hash::NONE => NodeId::ROOT,
                        Some(h) => txn.get_internal(h).unwrap_or(None).unwrap_or(NodeId::ROOT),
                    };
                    let inode_pos = Position::new(inode_change_id, add.inode.pos);

                    if let Ok(Some(inode)) = txn.position_inode(inode_pos) {
                        if let Ok(Some(old_path)) = txn.get_path(inode) {
                            if old_path != *path {
                                let _ = txn.del_tree(&old_path);
                            }
                        }
                        let _ = txn.put_tree(path, inode);
                    }
                }
                _ => {}
            }
        }

        if !preserve_existing_tree_paths {
            for deleted_path in deleted_paths {
                if let Ok(Some(inode)) = txn.get_inode(deleted_path) {
                    let dominated = is_file_only_on_view(&txn, inode, view_name);
                    if dominated {
                        let _ = txn.del_tree(deleted_path);
                        let _ = txn.del_inode(inode);
                    }
                }
            }
        }

        let apply_start = std::time::Instant::now();
        let (insert, direct_graph_ms, direct_crdt_ms) = if import_direct_can_apply(&final_change) {
            let (insert, graph_ms, crdt_ms) = self.write_import_direct_add_chain(
                &mut txn,
                view_name,
                change_id,
                &hash,
                &final_change,
                &options,
            )?;
            (insert, graph_ms, crdt_ms)
        } else {
            let insert = write_change_to_graph(
                &mut txn,
                view_name,
                change_id,
                &hash,
                &final_change,
                &options,
                false,
            )
            .map_err(|e| RepositoryError::Apply(e.to_string()))?;
            (insert, 0, 0)
        };
        timings.apply_ms = apply_start.elapsed().as_millis();
        timings.direct_graph_ms = direct_graph_ms;
        timings.direct_crdt_ms = direct_crdt_ms;

        let commit_start = std::time::Instant::now();
        self.append_deferred_tree_ops(&txn, &tree_ops, view_name, preserve_existing_tree_paths)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        timings.commit_ms = commit_start.elapsed().as_millis();

        Ok(ImportWriteOutcome {
            hash,
            timings,
            insert,
        })
    }

    /// Save and apply an already-built git-import graph change.
    ///
    /// This bypasses record/globalize and eager CRDT table writes. The caller
    /// has already compiled Git's snapshot delta into line-level graph ops.
    /// `preserve_existing_tree_paths` keeps deletions and renames graph-only
    /// while a different view owns the materialized working copy.
    pub fn write_import_graph_change(
        &self,
        change: Change,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
        options: InsertOptions,
    ) -> Result<ImportWriteOutcome, RepositoryError> {
        let mut timings = ImportWriteTimings::default();
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let assemble_start = std::time::Instant::now();
        let mut v3_bytes = Vec::new();
        let hash = change
            .serialize(&mut v3_bytes)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let (final_change, verified_hash) = Change::deserialize(&mut v3_bytes.as_slice())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        debug_assert_eq!(hash, verified_hash);
        timings.assemble_ms = assemble_start.elapsed().as_millis();

        let save_start = std::time::Instant::now();
        self.save_change_bytes(&hash, &v3_bytes, &final_change)?;
        timings.save_ms = save_start.elapsed().as_millis();

        let change_id = txn
            .register_change(&hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.put_change_deps(change_id, final_change.dependencies())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let tree_ops = collect_tree_ops(&txn, hash, &final_change, deleted_paths)?;

        let apply_start = std::time::Instant::now();
        let insert = if import_graph_first_can_apply(&final_change) {
            let (insert, graph_ms, crdt_ms) = self.write_import_graph_first_direct(
                &mut txn,
                view_name,
                change_id,
                &hash,
                &final_change,
                preserve_existing_tree_paths,
            )?;
            timings.direct_graph_ms = graph_ms;
            timings.direct_crdt_ms = crdt_ms;
            insert
        } else {
            write_change_to_graph(
                &mut txn,
                view_name,
                change_id,
                &hash,
                &final_change,
                &options,
                false,
            )
            .map_err(|e| RepositoryError::Apply(e.to_string()))?
        };
        timings.apply_ms = apply_start.elapsed().as_millis();

        if !preserve_existing_tree_paths {
            for deleted_path in deleted_paths {
                if let Ok(Some(inode)) = txn.get_inode(deleted_path) {
                    let _ = txn.del_tree(deleted_path);
                    if is_file_only_on_view(&txn, inode, view_name) {
                        let _ = txn.del_inode(inode);
                    }
                }
            }
        }

        let commit_start = std::time::Instant::now();
        self.append_deferred_tree_ops(&txn, &tree_ops, view_name, preserve_existing_tree_paths)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        timings.commit_ms = commit_start.elapsed().as_millis();

        Ok(ImportWriteOutcome {
            hash,
            timings,
            insert,
        })
    }

    fn write_import_graph_first_direct(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        view_name: &str,
        change_id: NodeId,
        hash: &Hash,
        change: &Change,
        preserve_existing_tree_paths: bool,
    ) -> Result<(InsertOutcome, u128, u128), RepositoryError> {
        use atomic_core::apply::compute_new_state;

        let graph_start = std::time::Instant::now();
        let mut pending_edges: Vec<PendingImportEdge> = Vec::new();

        {
            let mut old_by_end: HashMap<Position<NodeId>, GraphNode<NodeId>> = HashMap::new();
            let mut current_by_end: HashMap<Position<NodeId>, GraphNode<NodeId>> = HashMap::new();
            let mut current_by_start: HashMap<Position<NodeId>, GraphNode<NodeId>> = HashMap::new();
            let mut vertex_cache = ImportGraphFirstVertexCache::default();

            for graph_op in change.hunks() {
                match graph_op {
                    GraphOp::FileAdd {
                        add_name,
                        add_inode,
                        contents,
                        path,
                        ..
                    } => {
                        let inode_position = Position::new(change_id, add_inode.start);
                        let inode = txn
                            .alloc_inode()
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        if !preserve_existing_tree_paths {
                            txn.put_tree(path, inode)
                                .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        }
                        txn.put_inode(inode, inode_position)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;

                        let name_node = GraphNode {
                            change: change_id,
                            start: add_name.start,
                            end: add_name.end,
                        };
                        let name_inode =
                            import_graph_first_resolved_inode(&*txn, &add_name.inode, change_id)?;
                        let name_source = import_graph_first_source(
                            &*txn,
                            &add_name.predecessors[0],
                            &add_name.inode,
                            name_inode,
                            &old_by_end,
                            &current_by_end,
                            &mut vertex_cache,
                            change_id,
                        )?;
                        pending_edges.push((
                            name_inode,
                            add_name.flag | EdgeFlags::BLOCK,
                            name_source,
                            name_node,
                        ));
                        current_by_end.insert(name_node.end_pos(), name_node);
                        current_by_start.insert(name_node.start_pos(), name_node);

                        let inode_node = GraphNode {
                            change: change_id,
                            start: add_inode.start,
                            end: add_inode.end,
                        };
                        let inode_source = import_graph_first_source(
                            &*txn,
                            &add_inode.predecessors[0],
                            &add_inode.inode,
                            Some(inode),
                            &old_by_end,
                            &current_by_end,
                            &mut vertex_cache,
                            change_id,
                        )?;
                        pending_edges.push((
                            Some(inode),
                            add_inode.flag | EdgeFlags::BLOCK,
                            inode_source,
                            inode_node,
                        ));
                        current_by_end.insert(inode_node.end_pos(), inode_node);
                        current_by_start.insert(inode_node.start_pos(), inode_node);

                        if let Some(contents) = contents {
                            let content_node = GraphNode {
                                change: change_id,
                                start: contents.start,
                                end: contents.end,
                            };
                            let content_source = import_graph_first_source(
                                &*txn,
                                &contents.predecessors[0],
                                &contents.inode,
                                Some(inode),
                                &old_by_end,
                                &current_by_end,
                                &mut vertex_cache,
                                change_id,
                            )?;
                            pending_edges.push((
                                Some(inode),
                                contents.flag | EdgeFlags::BLOCK,
                                content_source,
                                content_node,
                            ));
                            current_by_end.insert(content_node.end_pos(), content_node);
                            current_by_start.insert(content_node.start_pos(), content_node);
                        }
                    }
                    GraphOp::Replacement {
                        change: edge_update,
                        replacement,
                        ..
                    } => {
                        let resolved_inode = import_graph_first_resolved_inode(
                            &*txn,
                            &edge_update.inode,
                            change_id,
                        )?;

                        for edge in &edge_update.edges {
                            let target = import_graph_first_node(&*txn, edge.to, change_id)?;
                            let source = import_graph_first_source(
                                &*txn,
                                &edge.from,
                                &edge_update.inode,
                                resolved_inode,
                                &old_by_end,
                                &current_by_end,
                                &mut vertex_cache,
                                change_id,
                            )?;
                            pending_edges.push((resolved_inode, edge.flag, source, target));
                            old_by_end.insert(target.end_pos(), target);
                        }

                        let node = GraphNode {
                            change: change_id,
                            start: replacement.start,
                            end: replacement.end,
                        };
                        let source = import_graph_first_source(
                            &*txn,
                            &replacement.predecessors[0],
                            &replacement.inode,
                            resolved_inode,
                            &old_by_end,
                            &current_by_end,
                            &mut vertex_cache,
                            change_id,
                        )?;
                        pending_edges.push((
                            resolved_inode,
                            replacement.flag | EdgeFlags::BLOCK,
                            source,
                            node,
                        ));

                        for successor in &replacement.successors {
                            let target = import_graph_first_successor(
                                &*txn,
                                successor,
                                resolved_inode,
                                &current_by_start,
                                &mut vertex_cache,
                                change_id,
                            )?;
                            pending_edges.push((
                                resolved_inode,
                                replacement.flag | EdgeFlags::BLOCK,
                                node,
                                target,
                            ));
                        }

                        current_by_end.insert(node.end_pos(), node);
                        current_by_start.insert(node.start_pos(), node);
                    }
                    GraphOp::FileMove { del, add, path } => {
                        let resolved_inode =
                            import_graph_first_resolved_inode(&*txn, &add.inode, change_id)?;

                        for edge in &del.edges {
                            let target = import_graph_first_node(&*txn, edge.to, change_id)?;
                            let source = import_graph_first_source(
                                &*txn,
                                &edge.from,
                                &del.inode,
                                resolved_inode,
                                &old_by_end,
                                &current_by_end,
                                &mut vertex_cache,
                                change_id,
                            )?;
                            pending_edges.push((resolved_inode, edge.flag, source, target));
                            old_by_end.insert(target.end_pos(), target);
                        }

                        let node = GraphNode {
                            change: change_id,
                            start: add.start,
                            end: add.end,
                        };
                        let source = import_graph_first_source(
                            &*txn,
                            &add.predecessors[0],
                            &add.inode,
                            resolved_inode,
                            &old_by_end,
                            &current_by_end,
                            &mut vertex_cache,
                            change_id,
                        )?;
                        pending_edges.push((
                            resolved_inode,
                            add.flag | EdgeFlags::BLOCK,
                            source,
                            node,
                        ));

                        let inode_pos = import_graph_first_position(&*txn, &add.inode, change_id)?;
                        for successor in &add.successors {
                            let resolved_successor =
                                import_graph_first_position(&*txn, successor, change_id)?;
                            let target = if resolved_successor == inode_pos {
                                GraphNode {
                                    change: inode_pos.change,
                                    start: inode_pos.pos,
                                    end: inode_pos.pos,
                                }
                            } else {
                                import_graph_first_successor(
                                    &*txn,
                                    successor,
                                    resolved_inode,
                                    &current_by_start,
                                    &mut vertex_cache,
                                    change_id,
                                )?
                            };
                            pending_edges.push((
                                resolved_inode,
                                add.flag | EdgeFlags::BLOCK,
                                node,
                                target,
                            ));
                        }

                        if !preserve_existing_tree_paths {
                            if let Some(inode) = resolved_inode {
                                if let Ok(Some(old_path)) = txn.get_path(inode) {
                                    if old_path != *path {
                                        let _ = txn.del_tree(&old_path);
                                    }
                                }
                                txn.put_tree(path, inode)
                                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                            }
                        }

                        current_by_end.insert(node.end_pos(), node);
                        current_by_start.insert(node.start_pos(), node);
                    }
                    GraphOp::Edit {
                        change: atomic_core::change::Atom::Insertion(insertion),
                        ..
                    } => {
                        let resolved_inode =
                            import_graph_first_resolved_inode(&*txn, &insertion.inode, change_id)?;
                        let node = GraphNode {
                            change: change_id,
                            start: insertion.start,
                            end: insertion.end,
                        };
                        let source = import_graph_first_source(
                            &*txn,
                            &insertion.predecessors[0],
                            &insertion.inode,
                            resolved_inode,
                            &old_by_end,
                            &current_by_end,
                            &mut vertex_cache,
                            change_id,
                        )?;
                        pending_edges.push((
                            resolved_inode,
                            insertion.flag | EdgeFlags::BLOCK,
                            source,
                            node,
                        ));

                        for successor in &insertion.successors {
                            let target = import_graph_first_successor(
                                &*txn,
                                successor,
                                resolved_inode,
                                &current_by_start,
                                &mut vertex_cache,
                                change_id,
                            )?;
                            pending_edges.push((
                                resolved_inode,
                                insertion.flag | EdgeFlags::BLOCK,
                                node,
                                target,
                            ));
                        }

                        current_by_end.insert(node.end_pos(), node);
                        current_by_start.insert(node.start_pos(), node);
                    }
                    GraphOp::Edit {
                        change: atomic_core::change::Atom::EdgeUpdate(edge_update),
                        ..
                    } => {
                        let resolved_inode = import_graph_first_resolved_inode(
                            &*txn,
                            &edge_update.inode,
                            change_id,
                        )?;

                        for edge in &edge_update.edges {
                            let target = import_graph_first_node(&*txn, edge.to, change_id)?;
                            let source = import_graph_first_source(
                                &*txn,
                                &edge.from,
                                &edge_update.inode,
                                resolved_inode,
                                &old_by_end,
                                &current_by_end,
                                &mut vertex_cache,
                                change_id,
                            )?;
                            pending_edges.push((resolved_inode, edge.flag, source, target));
                            old_by_end.insert(target.end_pos(), target);
                        }
                    }
                    _ => {
                        return Err(RepositoryError::Apply(
                            "graph-first direct import received unsupported graph op".to_string(),
                        ));
                    }
                }
            }
        }

        {
            let mut graph_batch = atomic_core::apply::CachedWriteGraphTxn::new(&*txn)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            for (inode, flag, source, target) in pending_edges {
                graph_batch
                    .add_edge_with_reverse(inode, flag, source, target, change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }
        let graph_ms = graph_start.elapsed().as_millis();

        // Phase 1 of git import writes the graph truth and stores semantic
        // FileOps in the change file, but intentionally does not fan those
        // FileOps out into CRDT tables. That table materialization is phase 2.
        let crdt_ms = 0;

        let mut view = txn
            .open_or_create_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let new_state = compute_new_state(&view.state, hash);
        let sequence = view.change_count + 1;
        txn.put_change(&mut view, change_id, hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        view.state = new_state;
        view.change_count = sequence;
        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut stats = InsertStats::new();
        stats.changes_applied = 1;
        stats.applied_hashes.push(*hash);
        stats.atoms_processed = change.hunks().len();

        Ok((
            InsertOutcome::new(new_state, sequence, false, stats),
            graph_ms,
            crdt_ms,
        ))
    }

    fn write_import_direct_add_chain(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        view_name: &str,
        change_id: NodeId,
        hash: &Hash,
        change: &Change,
        _options: &InsertOptions,
    ) -> Result<(InsertOutcome, u128, u128), RepositoryError> {
        use atomic_core::apply::{apply_file_ops_batched, compute_new_state};

        let mut by_end: HashMap<ChangePosition, GraphNode<NodeId>> = HashMap::new();
        let mut inode_by_pos: HashMap<ChangePosition, Inode> = HashMap::new();

        for graph_op in change.hunks() {
            if let GraphOp::FileAdd {
                add_inode, path, ..
            } = graph_op
            {
                let inode_position = Position::new(change_id, add_inode.start);
                let inode = match txn
                    .position_inode(inode_position)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                {
                    Some(existing) => existing,
                    None => {
                        let inode = txn
                            .alloc_inode()
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_tree(path, inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_inode(inode, inode_position)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        inode
                    }
                };
                inode_by_pos.insert(add_inode.start, inode);
            }
        }

        let graph_start = std::time::Instant::now();
        {
            let mut batch = atomic_core::apply::CachedWriteGraphTxn::new(&*txn)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut inode_sources: HashSet<(u64, GraphNode<NodeId>)> = HashSet::new();
            let mut inode_terminal_candidates: Vec<(
                Inode,
                GraphNode<NodeId>,
                SerializedGraphEdge,
            )> = Vec::new();

            for graph_op in change.hunks() {
                match graph_op {
                    GraphOp::FileAdd {
                        add_name,
                        add_inode,
                        contents,
                        ..
                    } => {
                        import_direct_write_insertion(
                            &mut batch,
                            change_id,
                            add_name,
                            &mut by_end,
                            &inode_by_pos,
                            &mut inode_sources,
                            &mut inode_terminal_candidates,
                        )?;
                        import_direct_write_insertion(
                            &mut batch,
                            change_id,
                            add_inode,
                            &mut by_end,
                            &inode_by_pos,
                            &mut inode_sources,
                            &mut inode_terminal_candidates,
                        )?;
                        if let Some(contents) = contents {
                            import_direct_write_insertion(
                                &mut batch,
                                change_id,
                                contents,
                                &mut by_end,
                                &inode_by_pos,
                                &mut inode_sources,
                                &mut inode_terminal_candidates,
                            )?;
                        }
                    }
                    GraphOp::Edit {
                        change: atomic_core::change::Atom::Insertion(insertion),
                        ..
                    } => {
                        import_direct_write_insertion(
                            &mut batch,
                            change_id,
                            insertion,
                            &mut by_end,
                            &inode_by_pos,
                            &mut inode_sources,
                            &mut inode_terminal_candidates,
                        )?;
                    }
                    _ => {
                        return Err(RepositoryError::Apply(
                            "direct import received unsupported graph op".to_string(),
                        ));
                    }
                }
            }

            for (inode, node, edge) in inode_terminal_candidates {
                if !inode_sources.contains(&(inode.get(), node)) {
                    batch
                        .put_inode_graph(inode, node, edge)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
            }
        }
        let graph_ms = graph_start.elapsed().as_millis();

        let crdt_start = std::time::Instant::now();
        if change.has_file_ops() {
            apply_file_ops_batched(txn, change_id, change.file_ops())
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        let crdt_ms = crdt_start.elapsed().as_millis();

        let mut view = txn
            .open_or_create_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let new_state = compute_new_state(&view.state, hash);
        let sequence = view.change_count + 1;
        txn.put_change(&mut view, change_id, hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        view.state = new_state;
        view.change_count = sequence;
        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut stats = InsertStats::new();
        stats.changes_applied = 1;
        stats.applied_hashes.push(*hash);
        stats.atoms_processed = change.hunks().len();

        Ok((
            InsertOutcome::new(new_state, sequence, false, stats),
            graph_ms,
            crdt_ms,
        ))
    }

    /// Insert a change into the current view.
    ///
    /// This is the high-level method for inserting a single change into the
    /// repository. It loads the change from the change store, validates
    /// dependencies, applies atoms to the graph, and updates the view state.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to insert
    /// * `options` - Options controlling insertion behavior
    ///
    /// # Returns
    ///
    /// An `InsertOutcome` containing the new state and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change is not found in the change store
    /// - Dependencies are missing (unless `apply_dependencies` is set)
    /// - The change is already inserted
    /// - A conflict occurs (unless `allow_conflicts` is set)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::{Repository, InsertOptions};
    ///
    /// let repo = Repository::open(".")?;
    /// let result = repo.insert_change(&hash, InsertOptions::default())?;
    /// println!("New state: {}", result.new_state.to_base32());
    /// ```
    pub fn insert_change(
        &self,
        hash: &Hash,
        options: InsertOptions,
    ) -> Result<InsertOutcome, RepositoryError> {
        let trace_insert = std::env::var_os("ATOMIC_TRACE_INSERT").is_some();
        let t0 = std::time::Instant::now();

        // Load the change from the store
        let change = self.load_change(hash)?;

        if trace_insert {
            eprintln!(
                "[insert_change] hash={} load_change elapsed={:?} hunks={} deps={}",
                &hash.to_base32()[..12],
                t0.elapsed(),
                change.hunks().len(),
                change.dependencies().len(),
            );
        }

        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if the change's edges are already in the global GRAPH.
        //
        // A change is "already in the global graph" when it is registered
        // (has a NodeId) AND at least one of its vertices exists in the
        // GRAPH B-tree.  `has_change_in_graph` performs a single O(log N)
        // range scan — far cheaper and more reliable than the previous
        // approach of loading the Change file and probing individual hunks.
        //
        // This correctly handles:
        //   - Changes recorded on a Draft view (edges in GRAPH only)
        //     → returns false, so hunks are re-applied to the global GRAPH
        //   - Changes already inserted into a Shared view (edges in GRAPH)
        //     → returns true, so redundant hunk application is skipped
        //   - Changes with only EdgeUpdate hunks (no FileAdd/DirAdd)
        //     → correctly detected via the range scan
        let t_check = std::time::Instant::now();
        let already_in_graph = if let Some(node_id) = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let in_graph = txn
                .has_change_in_graph(node_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            log::debug!(
                "insert_change: hash={} node_id={:?} already_in_graph={}",
                hash.to_base32(),
                node_id,
                in_graph
            );
            in_graph
        } else {
            log::debug!(
                "insert_change: hash={} not in INTERNAL (new change)",
                hash.to_base32()
            );
            false
        };

        if trace_insert {
            eprintln!(
                "[insert_change] hash={} already_in_graph={} check elapsed={:?}",
                &hash.to_base32()[..12],
                already_in_graph,
                t_check.elapsed(),
            );
        }

        // Register the change to get an internal ID (or get existing ID).
        // (If get_internal succeeded above, register_change just returns
        // the existing ID without re-registering.)
        let change_id = txn
            .register_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.put_change_deps(change_id, change.dependencies())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which view to use
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);
        let preserve_existing_tree_paths = view_name != self.current_view;
        log::debug!(
            "insert_change: change_id={:?} view={} already_in_graph={} hunks={}",
            change_id,
            view_name,
            already_in_graph,
            change.hunks().len()
        );
        let tree_ops = collect_tree_ops(&txn, *hash, &change, &[])?;

        // Populate tree tables for FileAdd/DirAdd/FileDel hunks.
        // This creates the path→inode→position mappings that materialize
        // needs to reconstruct files. Without this, server-side repos (which
        // receive changes via push rather than record) would have an empty tree.
        let t_tree = std::time::Instant::now();
        if !already_in_graph {
            for graph_op in change.hunks() {
                match graph_op {
                    GraphOp::FileAdd {
                        add_inode, path, ..
                    } => {
                        let new_inode = txn
                            .alloc_inode()
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        let inode_position = Position::new(change_id, add_inode.start);
                        if !preserve_existing_tree_paths {
                            txn.put_tree(path, new_inode)
                                .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        }
                        txn.put_inode(new_inode, inode_position)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    GraphOp::DirAdd {
                        add_inode, path, ..
                    } => {
                        use atomic_core::pristine::directory_flags;
                        let new_inode = txn
                            .alloc_inode()
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        let inode_position = Position::new(change_id, add_inode.start);
                        if !preserve_existing_tree_paths {
                            txn.put_tree(path, new_inode)
                                .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        }
                        txn.put_inode(new_inode, inode_position)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_directory(new_inode, directory_flags::explicit_empty())
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    GraphOp::FileDel { path, .. } if !preserve_existing_tree_paths => {
                        // View-aware: only remove TREE entry when no other
                        // view still references the file's creating change.
                        if let Ok(Some(inode)) = txn.get_inode(path) {
                            let dominated = is_file_only_on_view(&txn, inode, view_name);
                            if dominated {
                                let _ = txn.del_tree(path);
                            }
                        }
                    }
                    // NOTE: FileMove TREE maintenance is handled unconditionally
                    // below (not gated by `!already_in_graph`), because a
                    // draft-recorded rename inserted cross-view is always
                    // already-ambient in GRAPH and would otherwise be skipped
                    // here — leaving TREE pointed at the old path so the rename
                    // never materializes (rubric A10, ATOM::36).
                    _ => {}
                }
            }

            if trace_insert {
                eprintln!(
                    "[insert_change] hash={} tree_tables elapsed={:?}",
                    &hash.to_base32()[..12],
                    t_tree.elapsed(),
                );
            }
        }

        // FileMove TREE maintenance for the CURRENT view — ALWAYS (even when the
        // change's edges are already ambient in GRAPH). Inserting a rename must
        // repoint TREE old→new now so the caller's materialize produces the new
        // path; the eager `!already_in_graph` block above only runs for brand-new
        // changes and misses the common cross-view-insert case. Old on-disk
        // paths are collected and removed after commit (materialize writes the
        // new path but never removes the old one), mirroring the FileDel
        // working-copy cleanup below. The deferred-tree journal append still
        // happens, so a later view switch replays to the same TREE state
        // idempotently.
        let mut moved_from_disk: Vec<String> = Vec::new();
        if !preserve_existing_tree_paths {
            for graph_op in change.hunks() {
                if let GraphOp::FileMove { add, path, .. } = graph_op {
                    // add.inode is Position<Option<Hash>>; resolve to Position<NodeId>.
                    let inode_change_id = match &add.inode.change {
                        None => change_id,
                        Some(h) if *h == Hash::NONE => NodeId::ROOT,
                        Some(h) => txn.get_internal(h).unwrap_or(None).unwrap_or(NodeId::ROOT),
                    };
                    let inode_pos = Position::new(inode_change_id, add.inode.pos);
                    if let Ok(Some(inode)) = txn.position_inode(inode_pos) {
                        if let Ok(Some(old_path)) = txn.get_path(inode) {
                            // Only repoint when the tracked path actually differs
                            // (guards against a prior FileMove in this same change
                            // already having updated it).
                            if old_path != *path {
                                let _ = txn.del_tree(&old_path);
                                moved_from_disk.push(old_path);
                            }
                        }
                        let _ = txn.put_tree(path, inode);
                    }
                }
            }
        }

        // Apply to the graph (skips hunk application if already_in_graph)
        let t_graph = std::time::Instant::now();
        let outcome = write_change_to_graph(
            &mut txn,
            view_name,
            change_id,
            hash,
            &change,
            &options,
            already_in_graph,
        )
        .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        if trace_insert {
            eprintln!(
                "[insert_change] hash={} write_graph elapsed={:?} atoms={}",
                &hash.to_base32()[..12],
                t_graph.elapsed(),
                outcome.stats.atoms_processed,
            );
        }

        // Commit the transaction
        log::debug!("insert_change: committing transaction...");
        let commit_start = std::time::Instant::now();
        self.append_deferred_tree_ops(&txn, &tree_ops, view_name, preserve_existing_tree_paths)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let commit_ms = commit_start.elapsed().as_millis();
        if trace_insert {
            eprintln!(
                "[insert_change] hash={} txn.commit elapsed={:?}",
                &hash.to_base32()[..12],
                commit_start.elapsed(),
            );
        }
        if commit_ms > 50 {
            log::warn!(
                "insert_change: SLOW txn.commit() took {}ms (change_id={:?})",
                commit_ms,
                change_id
            );
        } else {
            log::debug!("insert_change: txn.commit() took {}ms", commit_ms);
        }

        // Working-copy cleanup for whole-file deletions (FileDel hunks).
        //
        // TREE/INODES are global and only cleaned up when no other view still
        // references the file, so a delete inserted into the current view can
        // leave the (now dead) file's stale bytes on disk — materialize only
        // writes or skips, it never removes. Re-check the file's visible
        // content on this view AFTER the change is applied and remove the
        // stale working-copy file when the content is truly gone. A
        // delete-vs-modify merge where lines survive yields Some(content) and
        // is left for materialize to rewrite. Truncate-to-empty is recorded
        // as an Edit (not FileDel) and never reaches this path.
        if view_name == self.current_view {
            for graph_op in change.hunks() {
                if let GraphOp::FileDel { path, .. } = graph_op {
                    let gone = matches!(self.get_file_content_on_view(path, view_name), Ok(None));
                    if gone {
                        let abs = self.root.join(path);
                        if abs.is_file() {
                            let _ = std::fs::remove_file(&abs);
                        }
                        let _ = self.del_file_index(path);
                    }
                }
            }

            // Remove the stale source of each applied FileMove. TREE was
            // repointed old→new above, so materialize will write the new path
            // but never deletes the old one. Only remove when the old path is
            // truly untracked on this view now (guards an A12-style shared path).
            for old_path in &moved_from_disk {
                if matches!(self.get_file_inode(old_path), Ok(None)) {
                    let abs = self.root.join(old_path);
                    if abs.is_file() {
                        let _ = std::fs::remove_file(&abs);
                    }
                    let _ = self.del_file_index(old_path);
                }
            }
        }

        if trace_insert {
            eprintln!(
                "[insert_change] hash={} complete total_elapsed={:?}",
                &hash.to_base32()[..12],
                t0.elapsed(),
            );
        }

        Ok(outcome)
    }

    /// Insert a change with automatic dependency resolution.
    ///
    /// This method attempts to insert a change and all its missing dependencies.
    /// Dependencies are inserted in topological order (dependencies before
    /// dependents).
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to insert
    /// * `options` - Options controlling insertion behavior
    ///
    /// # Returns
    ///
    /// An `InsertOutcome` containing aggregate statistics for all inserted changes.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Any required change cannot be found
    /// - A cyclic dependency is detected
    /// - Maximum recursion depth is exceeded
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.insert_change_rec(&hash, InsertOptions::default())?;
    /// println!("Inserted {} changes", result.stats.changes_applied);
    /// ```
    pub fn insert_change_rec(
        &self,
        hash: &Hash,
        options: InsertOptions,
    ) -> Result<InsertOutcome, RepositoryError> {
        let trace_insert = std::env::var_os("ATOMIC_TRACE_INSERT").is_some();
        let t0 = std::time::Instant::now();

        // Load the target change to get its dependencies
        let _change = self.load_change(hash)?;

        // Get the view name
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);

        if trace_insert {
            eprintln!(
                "[insert_change_rec] start hash={} view={}",
                &hash.to_base32()[..12],
                view_name,
            );
        }

        // Get a read transaction to check what's already inserted
        let read_txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = read_txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        // Collect all needed changes (including the target)
        let mut to_insert = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(*hash);

        while let Some(current_hash) = queue.pop_front() {
            if visited.contains(&current_hash) {
                continue;
            }
            visited.insert(current_hash);

            // Check if already inserted
            if let Ok(Some(id)) = read_txn.get_internal(&current_hash) {
                if read_txn.get_change_seq(&view, id).ok().flatten().is_some() {
                    continue; // Already inserted
                }
            }

            // Load and queue dependencies
            let dep_change = self.load_change(&current_hash)?;
            for dep in dep_change.dependencies() {
                if !visited.contains(dep) {
                    queue.push_back(*dep);
                }
            }

            to_insert.push(current_hash);
        }

        drop(read_txn);

        // Reverse to get topological order (dependencies first)
        to_insert.reverse();

        if trace_insert {
            eprintln!(
                "[insert_change_rec] dep_resolution complete to_insert={} visited={} elapsed={:?}",
                to_insert.len(),
                visited.len(),
                t0.elapsed(),
            );
        }

        // Now insert all changes in order
        let mut aggregate_stats = InsertStats::new();
        let mut final_state = Merkle::ZERO;
        let mut final_sequence = 0u64;
        let mut has_conflicts = false;
        let total = to_insert.len();

        for (i, change_hash) in to_insert.iter().enumerate() {
            let change_start = std::time::Instant::now();
            let outcome = self.insert_change(change_hash, options.clone())?;
            if trace_insert {
                eprintln!(
                    "[insert_change_rec] applied {}/{} hash={} elapsed={:?}",
                    i + 1,
                    total,
                    &change_hash.to_base32()[..12],
                    change_start.elapsed(),
                );
            }
            aggregate_stats.merge(outcome.stats);
            final_state = outcome.new_state;
            final_sequence = outcome.sequence;
            if outcome.has_conflicts {
                has_conflicts = true;
            }
        }

        if trace_insert {
            eprintln!(
                "[insert_change_rec] complete total_inserted={} total_elapsed={:?}",
                total,
                t0.elapsed(),
            );
        }

        Ok(InsertOutcome::new(
            final_state,
            final_sequence,
            has_conflicts,
            aggregate_stats,
        ))
    }

    /// Write a recorded change to the repository.
    ///
    /// This method inserts a change that was just recorded, updating both the
    /// graph and the tree tables. It's the integration point between recording
    /// and inserting.
    ///
    /// Unlike `insert_change`, this method:
    /// - Takes the change directly (doesn't load from store)
    /// - Updates tree tables for FileAdd hunks
    /// - Assigns new inodes to added files
    ///
    /// # Arguments
    ///
    /// * `outcome` - The outcome from `record()` containing the change
    /// * `options` - Options controlling insertion behavior
    ///
    /// # Returns
    ///
    /// An `InsertOutcome` with the new state and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change has conflicts and `allow_conflicts` is false
    /// - Database operations fail
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let record_outcome = repo.record(header, options)?;
    /// let apply_outcome = repo.write_recorded(&record_outcome, InsertOptions::default())?;
    /// println!("Inserted with state: {}", apply_outcome.new_state.to_base32());
    /// ```
    pub fn write_recorded(
        &self,
        outcome: &RecordOutcome,
        mut options: InsertOptions,
    ) -> Result<InsertOutcome, RepositoryError> {
        let trace_record = std::env::var_os("ATOMIC_TRACE_RECORD").is_some();
        let change = outcome.change();
        let hash = outcome.hash();

        // A freshly-recorded change is applied to the view it was recorded on.
        // Its dependency closure is complete by construction, so it cannot
        // produce zombie or missing-context conflicts. Disable conflict
        // detection to skip the per-hunk zombie/deleted-context graph scans
        // (the dominant apply cost on large changes); the graph written is
        // identical, only the (empty) conflict report is skipped.
        options.track_conflicts = false;

        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register the change to get an internal ID
        let change_id = txn
            .register_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.put_change_deps(change_id, change.dependencies())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which view to use
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);
        let preserve_existing_tree_paths = view_name != self.current_view;
        let tree_ops = collect_tree_ops(&txn, *hash, change, outcome.deleted_files())?;

        // Before applying atoms, set up tree entries for FileAdd hunks.
        // This creates the inode→position and path→inode mappings needed
        // for the graph operations.
        //
        // Note: put_tree creates both TREE and REV_TREE entries.
        //       put_inode creates both INODES and REV_INODES entries.
        for graph_op in change.hunks() {
            match graph_op {
                GraphOp::FileAdd {
                    add_inode, path, ..
                } => {
                    // Allocate a new inode for this file
                    let new_inode = txn
                        .alloc_inode()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    // The inode span position is relative to this change.
                    // Since add_inode.start is a ChangePosition within this change's content,
                    // we create an internal position using the change_id we just registered.
                    let inode_position = Position::new(change_id, add_inode.start);

                    // Add to tree tables:
                    // - put_tree: path ↔ inode (TREE and REV_TREE)
                    // - put_inode: inode ↔ position (INODES and REV_INODES)
                    if !preserve_existing_tree_paths {
                        txn.put_tree(path, new_inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    txn.put_inode(new_inode, inode_position)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
                GraphOp::DirAdd {
                    add_inode, path, ..
                } => {
                    use atomic_core::pristine::directory_flags;

                    // Allocate a new inode for this directory
                    let new_inode = txn
                        .alloc_inode()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    // The inode span position is relative to this change.
                    let inode_position = Position::new(change_id, add_inode.start);

                    // Add to tree tables:
                    // - put_tree: path ↔ inode (TREE and REV_TREE)
                    // - put_inode: inode ↔ position (INODES and REV_INODES)
                    // - put_directory: mark inode as directory (DIRECTORIES)
                    if !preserve_existing_tree_paths {
                        txn.put_tree(path, new_inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    txn.put_inode(new_inode, inode_position)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    txn.put_directory(new_inode, directory_flags::explicit_empty())
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
                GraphOp::FileDel { path, .. } if !preserve_existing_tree_paths => {
                    // View-aware deletion: only remove TREE/INODES entries
                    // when no OTHER view still references the file's creating
                    // change.  The TREE and INODES tables are global — removing
                    // an entry here would make the file invisible on every
                    // view, not just the one where the deletion was recorded.
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        let dominated = is_file_only_on_view(&txn, inode, view_name);
                        if dominated {
                            let _ = txn.del_tree(path);
                            let _ = txn.del_inode(inode);
                        }
                    }
                    // When other views still reference the file we leave
                    // TREE/INODES intact.  The deletion is represented in
                    // the graph via DELETED edges and will be honoured by
                    // materialize's change_filter / retrieve_graph.
                }
                GraphOp::DirDel { path, .. } if !preserve_existing_tree_paths => {
                    // Same view-aware logic as FileDel above.
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        let dominated = is_file_only_on_view(&txn, inode, view_name);
                        if dominated {
                            let _ = txn.del_tree(path);
                            let _ = txn.del_inode(inode);
                            let _ = txn.del_directory(inode);
                        }
                    }
                }
                GraphOp::FileMove { add, path, .. } if !preserve_existing_tree_paths => {
                    // A FileMove reuses the existing inode — look it up via
                    // the inode position stored in add.inode, then update
                    // TREE: remove the old path mapping and insert the new one.
                    let inode_change_id = match &add.inode.change {
                        None => change_id,
                        Some(h) if *h == Hash::NONE => NodeId::ROOT,
                        Some(h) => txn.get_internal(h).unwrap_or(None).unwrap_or(NodeId::ROOT),
                    };
                    let inode_pos = Position::new(inode_change_id, add.inode.pos);

                    if let Ok(Some(inode)) = txn.position_inode(inode_pos) {
                        if let Ok(Some(old_path)) = txn.get_path(inode) {
                            if old_path != *path {
                                let _ = txn.del_tree(&old_path);
                            }
                        }
                        let _ = txn.put_tree(path, inode);
                    }
                }
                _ => {}
            }
        }

        // Handle file deletions tracked in the outcome.
        // Since we use GraphOp::Edit with EdgeUpdate for deletions (not GraphOp::FileDel),
        // we need to explicitly remove deleted files from the tree tables.
        // View-aware: only remove if no other view still references the file.
        if !preserve_existing_tree_paths {
            for deleted_path in outcome.deleted_files() {
                if let Ok(Some(inode)) = txn.get_inode(deleted_path) {
                    let dominated = is_file_only_on_view(&txn, inode, view_name);
                    if dominated {
                        let _ = txn.del_tree(deleted_path);
                        let _ = txn.del_inode(inode);
                    }
                }
            }
        }

        // Apply to the graph
        // For write_recorded, the change is always new (just recorded), so
        // already_in_graph is always false.
        let apply_outcome = write_change_to_graph(
            &mut txn, view_name, change_id, hash, change, &options,
            false, // always_in_graph: freshly recorded changes are never in the graph yet
        )
        .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Commit the transaction
        let commit_start = std::time::Instant::now();
        self.append_deferred_tree_ops(&txn, &tree_ops, view_name, preserve_existing_tree_paths)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        if trace_record {
            eprintln!(
                "[write_recorded] txn.commit complete elapsed={:?}",
                commit_start.elapsed()
            );
        }

        Ok(apply_outcome)
    }

    // Cross-View Insert Methods

    /// Get all changes inserted into a view.
    ///
    /// Returns changes in order from oldest (sequence 0) to newest.
    ///
    /// # Arguments
    ///
    /// * `view_name` - Name of the view to query (None = current view)
    ///
    /// # Returns
    ///
    /// Vector of (sequence, hash) pairs.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let changes = repo.get_view_changes(None)?;
    /// for (seq, hash) in changes {
    ///     println!("#{}: {}", seq, hash.to_base32());
    /// }
    /// ```
    pub fn get_view_changes(
        &self,
        view_name: Option<&str>,
    ) -> Result<Vec<(u64, Hash)>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let name = view_name.unwrap_or(&self.current_view);
        let view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        get_view_changes_fn(&txn, &view).map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Get changes that are in one view but not another.
    ///
    /// This is useful for determining what needs to be inserted when
    /// merging or cherry-picking between views.
    ///
    /// # Arguments
    ///
    /// * `from_view` - Source view name
    /// * `to_view` - Target view name (None = current view)
    ///
    /// # Returns
    ///
    /// Vector of hashes that are in `from_view` but not in `to_view`,
    /// in dependency order.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Find what's in feature that's not in main
    /// let missing = repo.get_missing_changes_between("feature", Some("main"))?;
    /// println!("{} changes to insert", missing.len());
    /// ```
    pub fn get_missing_changes_between(
        &self,
        from_view: &str,
        to_view: Option<&str>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let from = txn
            .get_view(from_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: from_view.to_string(),
            })?;

        let to_name = to_view.unwrap_or(&self.current_view);
        let to = txn
            .get_view(to_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: to_name.to_string(),
            })?;

        get_missing_changes(&txn, &from, &to).map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Get changes up to a specific tag in a view.
    ///
    /// Returns all changes from sequence 0 up to and including the
    /// sequence where the tag was created.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag
    /// * `view_name` - View to search (None = use tag's view)
    ///
    /// # Returns
    ///
    /// Vector of change hashes up to the tagged state.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let changes = repo.get_changes_up_to_tag("v1.0.0", None)?;
    /// println!("{} changes in release", changes.len());
    /// ```
    pub fn get_changes_up_to_tag(
        &self,
        tag_name: &str,
        view_name: Option<&str>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        // Get the tag
        let tag = if let Some(view) = view_name {
            self.get_tag_from_view(tag_name, view)?
        } else {
            // Try current view first, then any view
            self.get_tag(tag_name)?.or(self.get_tag_any_view(tag_name)?)
        };

        let tag = tag.ok_or_else(|| RepositoryError::TagNotFound {
            name: tag_name.to_string(),
        })?;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&tag.view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: tag.view.clone(),
            })?;

        // Get changes up to and including the tag's sequence
        crate::apply::get_changes_up_to_seq(&txn, &view, tag.sequence)
            .map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Insert changes from one view into another.
    ///
    /// This is the main method for cross-view operations. It can:
    /// - Insert all missing changes from source to target
    /// - Insert only changes up to a specific tag
    /// - Insert only specific changes
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the cross-view insert
    ///
    /// # Returns
    ///
    /// A `CrossViewInsertOutcome` with details about what was inserted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Insert all changes from feature to main
    /// let options = CrossViewInsertOptions::new("feature", "main");
    /// let result = repo.insert_from_view(options)?;
    /// println!("Inserted {} changes", result.changes_applied);
    ///
    /// // Insert changes up to a tag
    /// let options = CrossViewInsertOptions::new("feature", "main")
    ///     .up_to_tag("v1.0.0");
    /// let result = repo.insert_from_view(options)?;
    /// ```
    pub fn insert_from_view(
        &self,
        options: CrossViewInsertOptions,
    ) -> Result<CrossViewInsertOutcome, RepositoryError> {
        let trace_insert = std::env::var_os("ATOMIC_TRACE_INSERT").is_some();
        let t0 = std::time::Instant::now();

        let mut outcome = CrossViewInsertOutcome::new();
        outcome.was_dry_run = options.dry_run;

        // Determine which changes to consider
        let source_changes = if !options.only_changes.is_empty() {
            // Use only specified changes
            options.only_changes.clone()
        } else if let Some(ref tag_name) = options.up_to_tag {
            // Get changes up to the tag
            self.get_changes_up_to_tag(tag_name, Some(&options.from_view))?
        } else {
            // Get all changes from source view
            self.get_view_changes(Some(&options.from_view))?
                .into_iter()
                .map(|(_, hash)| hash)
                .collect()
        };

        if trace_insert {
            eprintln!(
                "[insert_from_view] start from={} to={} source_changes={}",
                options.from_view,
                options.to_view,
                source_changes.len(),
            );
            eprintln!(
                "[insert_from_view] source_changes collected count={} elapsed={:?}",
                source_changes.len(),
                t0.elapsed(),
            );
        }

        // Filter to changes not already in target
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let to_view = txn
            .get_view(&options.to_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: options.to_view.clone(),
            })?;

        let missing = filter_missing_in_view(&txn, &to_view, &source_changes)
            .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Track skipped changes
        let missing_set: std::collections::HashSet<_> = missing.iter().collect();
        for hash in &source_changes {
            if !missing_set.contains(hash) {
                outcome.skipped_hashes.push(*hash);
            }
        }

        if trace_insert {
            eprintln!(
                "[insert_from_view] filter_missing complete missing={} skipped={} elapsed={:?}",
                missing.len(),
                outcome.skipped_hashes.len(),
                t0.elapsed(),
            );
        }

        drop(txn);

        if missing.is_empty() {
            // Nothing to insert
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let view = txn
                .get_view(&options.to_view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .unwrap();
            outcome.new_state = view.state;
            outcome.sequence = view.change_count;
            return Ok(outcome);
        }

        // If dry run, just return what would be inserted
        if options.dry_run {
            outcome.applied_hashes = missing;
            outcome.changes_applied = outcome.applied_hashes.len();
            return Ok(outcome);
        }

        // When the source view is Draft, its changes were recorded against
        // the view filter (GRAPH).  Inserting those changes
        // into a different view verifies edge context against a different
        // graph view, which produces spurious "missing context" conflicts.
        // These are architecturally expected — not real data conflicts —
        // so we automatically allow them for cross-view insert.
        let source_is_draft = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.get_view(&options.from_view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .map(|s| s.kind.is_draft())
                .unwrap_or(false)
        };

        let apply_opts = InsertOptions::default()
            .view(&options.to_view)
            .allow_conflict(options.allow_conflicts || source_is_draft);

        let total_missing = missing.len();
        for (i, hash) in missing.iter().enumerate() {
            let change_start = std::time::Instant::now();

            let result = if options.apply_dependencies {
                self.insert_change_rec(hash, apply_opts.clone())
            } else {
                self.insert_change(hash, apply_opts.clone())
            };

            match result {
                Ok(apply_outcome) => {
                    if trace_insert {
                        let change_elapsed = change_start.elapsed();
                        eprintln!(
                            "[insert_from_view] change {}/{} hash={} elapsed={:?} cumulative={:?}",
                            i + 1,
                            total_missing,
                            &hash.to_base32()[..12],
                            change_elapsed,
                            t0.elapsed(),
                        );
                        if change_elapsed > std::time::Duration::from_millis(200) {
                            eprintln!(
                                "[insert_from_view] SLOW change hash={} took {:?}",
                                &hash.to_base32()[..12],
                                change_elapsed,
                            );
                        }
                    }

                    outcome.applied_hashes.push(*hash);
                    outcome.changes_applied += 1;
                    outcome.new_state = apply_outcome.new_state;
                    outcome.sequence = apply_outcome.sequence;
                    if apply_outcome.has_conflicts {
                        outcome.has_conflicts = true;
                    }
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        if trace_insert {
            eprintln!(
                "[insert_from_view] complete applied={} skipped={} conflicts={} total_elapsed={:?}",
                outcome.changes_applied,
                outcome.skipped_hashes.len(),
                outcome.has_conflicts,
                t0.elapsed(),
            );
        }

        Ok(outcome)
    }

    /// Insert changes up to a tag from one view into another.
    ///
    /// This is a convenience method that combines `get_changes_up_to_tag`
    /// and `insert_from_view`.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag to insert up to
    /// * `from_view` - Source view containing the tag
    /// * `to_view` - Target view (None = current view)
    ///
    /// # Returns
    ///
    /// A `CrossViewInsertOutcome` with details about what was inserted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Insert release-1.0.0 from feature to main
    /// let result = repo.insert_tag_to_view("release-1.0.0", "feature", Some("main"))?;
    /// ```
    pub fn insert_tag_to_view(
        &self,
        tag_name: &str,
        from_view: &str,
        to_view: Option<&str>,
    ) -> Result<CrossViewInsertOutcome, RepositoryError> {
        let target = to_view.unwrap_or(&self.current_view);

        let options = CrossViewInsertOptions::new(from_view, target)
            .up_to_tag(tag_name)
            .with_dependencies(true);

        self.insert_from_view(options)
    }

    /// Cherry-pick specific changes from one view into another.
    ///
    /// # Arguments
    ///
    /// * `changes` - Hashes of changes to insert
    /// * `from_view` - Source view (for validation)
    /// * `to_view` - Target view (None = current view)
    ///
    /// # Returns
    ///
    /// A `CrossViewInsertOutcome` with details about what was inserted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.cherry_pick(&[hash1, hash2], "feature", None)?;
    /// ```
    pub fn cherry_pick(
        &self,
        changes: &[Hash],
        _from_view: &str,
        to_view: Option<&str>,
    ) -> Result<CrossViewInsertOutcome, RepositoryError> {
        let target = to_view.unwrap_or(&self.current_view);

        // For cherry-pick, we insert specific changes with dependencies
        let options = CrossViewInsertOptions::new("", target)
            .only_changes(changes.to_vec())
            .with_dependencies(true);

        self.insert_from_view(options)
    }

    /// Record a Git SHA → Atomic change mapping in the GIT_SHA_INDEX.
    ///
    /// Called after each commit is imported to enable O(1) incremental lookups.
    /// The `change_hash` is the Atomic Blake3 hash returned by write_import_*.
    pub fn index_git_sha(&self, git_sha: &str, change_hash: &Hash) -> Result<(), RepositoryError> {
        use atomic_core::pristine::GitShaIndexMutTxnT;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the entity_id for this change hash
        let entity_id = txn
            .get_internal(change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "Change hash not found in INTERNAL: {}",
                    change_hash.to_base32()
                ))
            })?;

        txn.put_git_sha(git_sha, entity_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Record missing Git SHA → Atomic change mappings in one transaction.
    ///
    /// Existing mappings are skipped. If every mapping is already indexed the
    /// write transaction is dropped without committing, keeping incremental
    /// no-op imports free of per-change database writes.
    pub fn index_git_shas(&self, mappings: &[(String, Hash)]) -> Result<usize, RepositoryError> {
        use atomic_core::pristine::{GitShaIndexMutTxnT, GitShaIndexTxnT};

        if mappings.is_empty() {
            return Ok(0);
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let mut inserted = 0;

        for (git_sha, change_hash) in mappings {
            if txn
                .has_git_sha(git_sha)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                continue;
            }

            let entity_id = txn
                .get_internal(change_hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "Change hash not found in INTERNAL: {}",
                        change_hash.to_base32()
                    ))
                })?;
            txn.put_git_sha(git_sha, entity_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            inserted += 1;
        }

        if inserted == 0 {
            return Ok(0);
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(inserted)
    }

    /// Check if a Git SHA has been indexed.
    pub fn has_git_sha(&self, git_sha: &str) -> Result<bool, RepositoryError> {
        use atomic_core::pristine::GitShaIndexTxnT;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.has_git_sha(git_sha)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Backfill the GIT_SHA_INDEX from existing imported changes.
    ///
    /// Scans all changes on the current view, looks for `unhashed.git.sha`,
    /// and populates the index. Idempotent — skips already-indexed SHAs.
    pub fn backfill_git_sha_index(&self) -> Result<usize, RepositoryError> {
        use crate::HistoryOptions;
        use atomic_core::pristine::GitShaIndexTxnT;

        let entries = self.log(HistoryOptions::default())?;
        let mut count = 0;

        for entry in &entries {
            if let Ok(change) = self.load_change(&entry.hash) {
                if let Some(ref unhashed) = change.unhashed {
                    if let Some(git) = unhashed.get("git") {
                        if let Some(sha) = git.get("sha").and_then(|v| v.as_str()) {
                            // Check if already indexed
                            {
                                let txn = self
                                    .pristine
                                    .read_txn()
                                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                                if txn.has_git_sha(sha).unwrap_or(false) {
                                    continue;
                                }
                            }
                            // Index it
                            if let Err(e) = self.index_git_sha(sha, &entry.hash) {
                                log::warn!(
                                    "Failed to index git SHA {}: {}",
                                    &sha[..8.min(sha.len())],
                                    e
                                );
                            } else {
                                count += 1;
                            }
                        }
                    }
                }
            }
        }

        Ok(count)
    }
}
