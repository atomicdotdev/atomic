//! Graph content output.
//!
//! This module provides the [`output_graph_content`] function, which writes the
//! content of an alive graph to a [`VertexBuffer`]. It handles conflict detection
//! and marker insertion for cyclic, order, and zombie conflicts.
//!
//! # Overview
//!
//! After retrieving an alive graph and computing its SCC ordering, this module
//! outputs the content in the correct order, inserting conflict markers where
//! necessary:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       Content Output Pipeline                            │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  AliveGraph         OrderResult           VertexBuffer                  │
//! │  ┌──────────┐      ┌──────────┐          ┌──────────┐                  │
//! │  │ Vertices │ ───► │ SCCs     │ ───────► │ Content  │                  │
//! │  │ Edges    │      │ Order    │          │ Markers  │                  │
//! │  │ Flags    │      │ Conflicts│          │          │                  │
//! │  └──────────┘      └──────────┘          └──────────┘                  │
//! │                                                                         │
//! │  For each SCC (in order):                                               │
//! │    1. Single span → output directly                                   │
//! │    2. Multiple vertices → output as cyclic conflict                     │
//! │    3. Zombie vertices → wrap in zombie markers                          │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Conflict Handling
//!
//! ## Cyclic Conflicts
//!
//! When an SCC contains multiple vertices, there's a cycle in the graph,
//! meaning no clear ordering exists. We output all vertices with conflict
//! markers:
//!
//! ```text
//! >>>>>>> 1 [cyclic]
//! Content from span A
//! ======= 1
//! Content from span B
//! <<<<<<< 1
//! ```
//!
//! ## Zombie Conflicts
//!
//! Zombie vertices are deleted content that still has live connections.
//! They're wrapped in special markers:
//!
//! ```text
//! >>>>>>> 1 [zombie]
//! Deleted but modified content
//! <<<<<<< 1
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::repo::output_graph_content;
//! use atomic_core::output::alive::{retrieve_graph, compute_order};
//!
//! // Retrieve and order the graph
//! let result = retrieve_graph(&txn, position, Default::default())?;
//! let order = compute_order(&mut result.graph);
//!
//! // Create a writer
//! let mut buffer = Vec::new();
//! let mut writer = ConflictWriter::new(&mut buffer, "file.rs", position);
//!
//! // Output the content
//! output_graph_content(&changes, &hash_fn, &result.graph, &order, &mut writer)?;
//! ```

use crate::change::ChangeStore;
use crate::merge::{ConflictGroup, MergeOutcome, ResolvedConflicts, SemanticMergeEngine};
use crate::output::alive::{AliveGraph, OrderResult, VertexId};
use crate::output::traits::VertexBuffer;
use crate::pristine::GraphTxnT;
use crate::types::{ChangePosition, GraphNode, Hash, NodeId};

use super::error::{OutputError, OutputResult};
use super::fork::detect_fork_conflicts;

// OUTPUT GRAPH CONTENT

/// Output the content of an alive graph to a span buffer.
///
/// This function traverses the graph in SCC order (as computed by
/// [`compute_order`](crate::output::alive::compute_order)) and writes each
/// span's content to the buffer. Conflicts are handled by:
///
/// - **Cyclic conflicts**: Multi-span SCCs are output with conflict markers
/// - **Zombie content**: Deleted vertices with live edges get zombie markers
///
/// # Arguments
///
/// * `changes` - Change store for retrieving span content
/// * `hash_fn` - Function to convert NodeId to Hash (for conflict markers)
/// * `graph` - The alive graph containing vertices to output
/// * `order` - The computed SCC ordering
/// * `buffer` - The span buffer to write to
///
/// # Returns
///
/// `Ok(())` on success, or an error if content retrieval or writing fails.
///
/// # Errors
///
/// Returns an error if:
/// - Content cannot be retrieved from the change store
/// - Writing to the buffer fails
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::repo::output_graph_content;
/// use atomic_core::change::MemoryChangeStore;
///
/// let changes = MemoryChangeStore::new();
/// let hash_fn = |id: NodeId| txn.get_external(id).ok().flatten();
///
/// output_graph_content(&changes, hash_fn, &graph, &order, &mut writer)?;
/// ```
///
/// # Algorithm
///
/// 1. Iterate over SCCs in topological order
/// 2. For each SCC:
///    - If single span: output content directly
///    - If multiple vertices: begin cyclic conflict, output each, end conflict
/// 3. For zombie vertices: wrap in zombie conflict markers
/// 4. Track and report any content retrieval errors
pub fn output_graph_content<C, F, V>(
    changes: &C,
    hash_fn: F,
    graph: &AliveGraph,
    order: &OrderResult,
    buffer: &mut V,
) -> OutputResult<()>
where
    C: ChangeStore,
    F: Fn(NodeId) -> Option<Hash>,
    V: VertexBuffer,
{
    // Track conflict IDs
    let mut conflict_id: usize = 0;

    // Track zombie state
    let mut in_zombie: Option<usize> = None;

    // Process SCCs in reverse order (Tarjan produces reverse topological order,
    // so we iterate in reverse to get forward topological order for correct output)
    for scc in order.sccs.iter().rev() {
        // Skip empty SCCs (shouldn't happen, but be safe)
        if scc.is_empty() {
            continue;
        }

        // Check if this is a cyclic conflict (multi-span SCC)
        let is_cyclic = scc.len() > 1;

        if is_cyclic {
            conflict_id += 1;
            buffer
                .begin_cyclic_conflict(conflict_id)
                .map_err(OutputError::io)?;
        }

        // Output each span in the SCC
        for (i, &vertex_id) in scc.iter().enumerate() {
            // Get span data
            let vertex_data = match graph.try_get_vertex(vertex_id) {
                Some(v) => v,
                None => continue,
            };

            let node = vertex_data.node;

            // Handle zombie state transitions
            let is_zombie = vertex_data.is_zombie();

            if is_zombie && in_zombie.is_none() {
                // Entering zombie region
                conflict_id += 1;
                in_zombie = Some(conflict_id);

                let hash = hash_fn(node.change);
                let hashes: Vec<Hash> = hash.into_iter().collect();
                let hashes_ref: Option<&[Hash]> = if hashes.is_empty() {
                    None
                } else {
                    Some(&hashes)
                };

                buffer
                    .begin_zombie_conflict(conflict_id, hashes_ref)
                    .map_err(OutputError::io)?;
            } else if !is_zombie {
                // Exiting zombie region if we were in one
                if let Some(zombie_id) = in_zombie.take() {
                    buffer
                        .end_zombie_conflict(zombie_id)
                        .map_err(OutputError::io)?;
                }
            }

            // For cyclic conflicts, add separator between vertices
            if is_cyclic && i > 0 {
                let hash = hash_fn(node.change);
                let hashes: Vec<Hash> = hash.into_iter().collect();
                let hashes_ref: Option<&[Hash]> = if hashes.is_empty() {
                    None
                } else {
                    Some(&hashes)
                };

                buffer
                    .conflict_next(conflict_id, hashes_ref)
                    .map_err(OutputError::io)?;
            }

            // Skip empty vertices
            let vertex_len = node.end.get() - node.start.get();
            if vertex_len == 0 {
                continue;
            }

            // Output the node content
            let get_contents = |buf: &mut [u8]| -> Result<(), std::io::Error> {
                changes
                    .get_contents(&hash_fn, node, buf)
                    .map(|_| ())
                    .map_err(|e| std::io::Error::other(e.to_string()))
            };

            buffer
                .output_line(node, get_contents)
                .map_err(OutputError::io)?;
        }

        // End cyclic conflict if we started one
        if is_cyclic {
            // Close any open zombie first
            if let Some(zombie_id) = in_zombie.take() {
                buffer
                    .end_zombie_conflict(zombie_id)
                    .map_err(OutputError::io)?;
            }

            buffer
                .end_cyclic_conflict(conflict_id)
                .map_err(OutputError::io)?;
        }
    }

    // Close any remaining zombie conflict
    if let Some(zombie_id) = in_zombie {
        buffer
            .end_zombie_conflict(zombie_id)
            .map_err(OutputError::io)?;
    }

    Ok(())
}

// RESOLVE CONFLICTS SEMANTICALLY

/// Attempt to resolve conflicts using the semantic merge engine.
///
/// Handles two kinds of conflict:
///
/// 1. **Cyclic conflicts** — multi-vertex SCCs where Tarjan found a cycle.
/// 2. **Fork conflicts** — a parent vertex with multiple children that
///    landed in *different* single-vertex SCCs (no ordering between them).
///    This is the most common conflict type when two agents edit the same
///    line concurrently.
///
/// For each resolved conflict the merged bytes are stored in the returned
/// [`ResolvedConflicts`] map keyed on the **first** child `VertexId`.
/// The remaining vertices are recorded in the skip set.
///
/// The caller should pass the returned map to
/// [`output_graph_content_resolved`] so that resolved conflicts are written
/// as plain content instead of conflict markers.
///
/// Failures are swallowed gracefully — if the engine cannot merge a
/// particular conflict the markers are left for the normal output path.
pub fn resolve_conflicts_semantically<T, C>(
    txn: &T,
    changes: &C,
    graph: &AliveGraph,
    order: &OrderResult,
) -> ResolvedConflicts
where
    T: GraphTxnT,
    C: ChangeStore,
{
    let mut resolved = ResolvedConflicts::new();
    let engine = SemanticMergeEngine::new(txn, changes);

    // 1. Resolve cyclic conflicts (multi-vertex SCCs)
    for scc in &order.sccs {
        if scc.len() <= 1 {
            continue;
        }

        let vertices: Vec<GraphNode<NodeId>> = scc
            .iter()
            .filter_map(|&vid| graph.try_get_vertex(vid))
            .map(|v| v.node)
            .collect();

        if vertices.len() < 2 {
            continue;
        }

        let group = ConflictGroup::new(vertices);

        match engine.try_merge(&group) {
            Ok(MergeOutcome::AutoMerged { content, .. }) => {
                log::info!(
                    "Semantic merge resolved cyclic conflict ({} vertices, {} bytes)",
                    scc.len(),
                    content.len(),
                );
                let first = scc[0];
                resolved.insert_merged(first, content);
                for &vid in &scc[1..] {
                    resolved.insert_skip(vid);
                }
            }
            Ok(MergeOutcome::Conflict { .. }) => {
                log::debug!("Semantic merge: true conflict in SCC, keeping markers");
            }
            Ok(MergeOutcome::NoCrdtData) | Ok(MergeOutcome::Clean(_)) => {}
            Err(e) => {
                log::warn!("Semantic merge failed for SCC: {}", e);
            }
        }
    }

    // 2. Detect and resolve fork conflicts
    let forks = detect_fork_conflicts(graph, order);
    for fork in &forks {
        let vertices: Vec<GraphNode<NodeId>> = fork
            .children
            .iter()
            .filter_map(|&vid| graph.try_get_vertex(vid))
            .map(|v| v.node)
            .collect();

        if vertices.len() < 2 {
            continue;
        }

        // Before treating this as a concurrent CRDT conflict, ask the
        // change DAG whether one side already supersedes the others.
        //
        // The byte-graph can legitimately wire two alive vertices at the
        // same logical position when one change was recorded *with the
        // other already applied* (e.g. an edit recorded after a merge,
        // touching a line that the merged-in change had already
        // rewritten).  Both vertices look alive and concurrent to the
        // walker, but the dependency DAG says the later change was
        // built knowing the earlier one and is meant to replace it.
        //
        // Picking the supersedor here keeps the resolution in the
        // semantic layer rather than guessing at the byte-graph level.
        if let Some(winner_idx) = supersedor_in_fork(txn, &fork.children, graph) {
            log::info!(
                "Fork resolved by change-DAG supersession: child {} wins ({} fork children)",
                winner_idx,
                fork.children.len(),
            );
            let winner_vid = fork.children[winner_idx];
            // Mark losers to skip so only the winner's vertex is emitted
            // through the normal traversal.
            for (idx, &vid) in fork.children.iter().enumerate() {
                if idx != winner_idx {
                    resolved.insert_skip(vid);
                }
            }
            let _ = winner_vid;
            continue;
        }

        let group = ConflictGroup::new(vertices).with_parent(graph.get_vertex(fork.parent).node);

        match engine.try_merge(&group) {
            Ok(MergeOutcome::AutoMerged { content, .. }) => {
                log::info!(
                    "Semantic merge resolved fork conflict ({} children, {} bytes)",
                    fork.children.len(),
                    content.len(),
                );
                resolved.insert_merged(fork.children[0], content);
                for &vid in &fork.children[1..] {
                    resolved.insert_skip(vid);
                }
            }
            Ok(MergeOutcome::Conflict { .. }) => {
                log::debug!(
                    "Semantic merge: true conflict at fork ({} children) \
                     — emitting conflict markers",
                    fork.children.len(),
                );
                resolved.insert_unresolved_fork(fork.children.clone());
            }
            Ok(MergeOutcome::NoCrdtData) | Ok(MergeOutcome::Clean(_)) => {
                // No CRDT data — still need to wrap the fork in markers
                // so both sides are visible to the user.
                resolved.insert_unresolved_fork(fork.children.clone());
            }
            Err(e) => {
                log::warn!("Semantic merge failed for fork: {}", e);
                resolved.insert_unresolved_fork(fork.children.clone());
            }
        }
    }

    resolved
}

/// If one fork child's introducing change transitively depends on every
/// other fork child's introducing change, return its index — that change
/// was recorded with full knowledge of the others and supersedes them.
///
/// Returns `None` when no single child dominates (the fork is genuinely
/// concurrent and needs marker / semantic-merge handling).
fn supersedor_in_fork<T: GraphTxnT>(
    txn: &T,
    children: &[crate::output::alive::VertexId],
    graph: &AliveGraph,
) -> Option<usize> {
    use std::collections::HashSet;

    // Collect each child's introducing change.  ROOT vertices have no
    // change ID; the dependency-DAG check doesn't apply to them.
    let changes: Vec<NodeId> = children
        .iter()
        .map(|&vid| graph.try_get_vertex(vid).map(|v| v.node.change))
        .collect::<Option<Vec<_>>>()?;

    if changes.iter().any(|c| c.is_root()) {
        return None;
    }

    // For each candidate winner, walk its dependency closure and verify
    // that every OTHER child's change appears in it (directly or
    // transitively).  The walk follows the indexed normal dependency
    // edges via `get_change_deps` (hash form) + `get_internal` to
    // resolve back to NodeIds.
    let closure_of = |start: NodeId| -> HashSet<NodeId> {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut stack: Vec<NodeId> = vec![start];
        seen.insert(start);
        while let Some(id) = stack.pop() {
            let deps = match txn.get_change_deps(id) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for dep_hash in deps {
                if let Ok(Some(dep_id)) = txn.get_internal(&dep_hash) {
                    if seen.insert(dep_id) {
                        stack.push(dep_id);
                    }
                }
            }
        }
        seen
    };

    for (idx, &cand) in changes.iter().enumerate() {
        let closure = closure_of(cand);
        let dominates_all_others = changes
            .iter()
            .enumerate()
            .all(|(j, &other)| j == idx || closure.contains(&other));
        if dominates_all_others {
            return Some(idx);
        }
    }

    None
}

// OUTPUT GRAPH CONTENT (RESOLVED)

/// Output graph content with semantic-merge resolution.
///
/// This is the merge-aware counterpart of [`output_graph_content`].  For
/// every SCC whose lead vertex appears in `resolved`, the merged bytes are
/// written directly and the remaining vertices are silently skipped.  All
/// other SCCs are handled identically to `output_graph_content`.
///
/// If `resolved` is empty this function behaves exactly like
/// [`output_graph_content`].
pub fn output_graph_content_resolved<C, F, V>(
    changes: &C,
    hash_fn: F,
    graph: &AliveGraph,
    order: &OrderResult,
    buffer: &mut V,
    resolved: &ResolvedConflicts,
) -> OutputResult<()>
where
    C: ChangeStore,
    F: Fn(NodeId) -> Option<Hash>,
    V: VertexBuffer,
{
    // Fast path: nothing was resolved and no unresolved forks.
    if resolved.is_empty() && resolved.unresolved_forks().is_empty() {
        return output_graph_content(changes, hash_fn, graph, order, buffer);
    }

    // Track conflict IDs
    let mut conflict_id: usize = 0;

    // Track zombie state
    let mut in_zombie: Option<usize> = None;

    // Track which fork-conflict vertices have already been emitted.
    let mut fork_emitted: std::collections::HashSet<VertexId> = std::collections::HashSet::new();

    for scc in order.sccs.iter().rev() {
        if scc.is_empty() {
            continue;
        }

        let is_cyclic = scc.len() > 1;

        // ── Resolved SCC: emit merged bytes, no conflict markers ──────
        if is_cyclic {
            if let Some(merged) = resolved.get_merged(scc[0]) {
                if !merged.is_empty() {
                    let synthetic = GraphNode::new(
                        NodeId::ROOT,
                        ChangePosition::new(0),
                        ChangePosition::new(merged.len() as u64),
                    );
                    // Clone into a local so the closure can own it
                    // (`output_line` takes `FnOnce`).
                    let bytes = merged.to_vec();
                    buffer
                        .output_line(synthetic, |buf: &mut [u8]| -> Result<(), std::io::Error> {
                            buf.copy_from_slice(&bytes);
                            Ok(())
                        })
                        .map_err(OutputError::io)?;
                }
                continue; // skip remaining vertices in this SCC
            }
        }

        // ── Fork-resolved: single-vertex SCC with merged or skipped content ─
        if !is_cyclic && scc.len() == 1 {
            let vid = scc[0];
            if resolved.should_skip(vid) {
                continue;
            }
            if let Some(merged) = resolved.get_merged(vid) {
                if !merged.is_empty() {
                    let synthetic = GraphNode::new(
                        NodeId::ROOT,
                        ChangePosition::new(0),
                        ChangePosition::new(merged.len() as u64),
                    );
                    let bytes = merged.to_vec();
                    buffer
                        .output_line(synthetic, |buf: &mut [u8]| -> Result<(), std::io::Error> {
                            buf.copy_from_slice(&bytes);
                            Ok(())
                        })
                        .map_err(OutputError::io)?;
                }
                continue;
            }

            // ── Unresolved fork conflict ────────────────────────────
            // When this vertex is part of an unresolved fork, emit the
            // entire group wrapped in conflict markers.  Skip if we have
            // already emitted it as part of an earlier fork group.
            if fork_emitted.contains(&vid) {
                continue;
            }
            if let Some(group) = resolved.fork_group_for(vid) {
                conflict_id += 1;
                buffer
                    .begin_conflict(conflict_id, None)
                    .map_err(OutputError::io)?;

                for (idx, &child_vid) in group.iter().enumerate() {
                    if idx > 0 {
                        let change_id = graph.try_get_vertex(child_vid).map(|v| v.node.change);
                        let hash = change_id.and_then(|cid| hash_fn(cid));
                        let hashes: Vec<Hash> = hash.into_iter().collect();
                        let href: Option<&[Hash]> = if hashes.is_empty() {
                            None
                        } else {
                            Some(&hashes)
                        };
                        buffer
                            .conflict_next(conflict_id, href)
                            .map_err(OutputError::io)?;
                    }

                    if let Some(vertex_data) = graph.try_get_vertex(child_vid) {
                        let node = vertex_data.node;
                        let vertex_len = node.end.get() - node.start.get();
                        if vertex_len > 0 {
                            let get_contents = |buf: &mut [u8]| -> Result<(), std::io::Error> {
                                changes
                                    .get_contents(&hash_fn, node, buf)
                                    .map(|_| ())
                                    .map_err(|e| std::io::Error::other(e.to_string()))
                            };
                            buffer
                                .output_line(node, get_contents)
                                .map_err(OutputError::io)?;
                        }
                    }
                    fork_emitted.insert(child_vid);
                }

                buffer.end_conflict(conflict_id).map_err(OutputError::io)?;
                continue;
            }
        }

        // ── Normal (unresolved) path — same logic as output_graph_content ─
        if is_cyclic {
            conflict_id += 1;
            buffer
                .begin_cyclic_conflict(conflict_id)
                .map_err(OutputError::io)?;
        }

        for (i, &vertex_id) in scc.iter().enumerate() {
            // Skip vertices that belong to a *different* resolved SCC
            // (shouldn't happen, but guard defensively).
            if resolved.should_skip(vertex_id) {
                continue;
            }

            let vertex_data = match graph.try_get_vertex(vertex_id) {
                Some(v) => v,
                None => continue,
            };

            let node = vertex_data.node;

            // Handle zombie state transitions
            let is_zombie = vertex_data.is_zombie();

            if is_zombie && in_zombie.is_none() {
                conflict_id += 1;
                in_zombie = Some(conflict_id);

                let hash = hash_fn(node.change);
                let hashes: Vec<Hash> = hash.into_iter().collect();
                let hashes_ref: Option<&[Hash]> = if hashes.is_empty() {
                    None
                } else {
                    Some(&hashes)
                };

                buffer
                    .begin_zombie_conflict(conflict_id, hashes_ref)
                    .map_err(OutputError::io)?;
            } else if !is_zombie {
                if let Some(zombie_id) = in_zombie.take() {
                    buffer
                        .end_zombie_conflict(zombie_id)
                        .map_err(OutputError::io)?;
                }
            }

            if is_cyclic && i > 0 {
                let hash = hash_fn(node.change);
                let hashes: Vec<Hash> = hash.into_iter().collect();
                let hashes_ref: Option<&[Hash]> = if hashes.is_empty() {
                    None
                } else {
                    Some(&hashes)
                };

                buffer
                    .conflict_next(conflict_id, hashes_ref)
                    .map_err(OutputError::io)?;
            }

            let vertex_len = node.end.get() - node.start.get();
            if vertex_len == 0 {
                continue;
            }

            let get_contents = |buf: &mut [u8]| -> Result<(), std::io::Error> {
                changes
                    .get_contents(&hash_fn, node, buf)
                    .map(|_| ())
                    .map_err(|e| std::io::Error::other(e.to_string()))
            };

            buffer
                .output_line(node, get_contents)
                .map_err(OutputError::io)?;
        }

        if is_cyclic {
            if let Some(zombie_id) = in_zombie.take() {
                buffer
                    .end_zombie_conflict(zombie_id)
                    .map_err(OutputError::io)?;
            }

            buffer
                .end_cyclic_conflict(conflict_id)
                .map_err(OutputError::io)?;
        }
    }

    if let Some(zombie_id) = in_zombie {
        buffer
            .end_zombie_conflict(zombie_id)
            .map_err(OutputError::io)?;
    }

    Ok(())
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{Change, ChangeHeader, MemoryChangeStore};
    use crate::output::alive::{AliveGraph, AliveVertex, OrderResult, VertexId};
    use crate::output::repo::ConflictWriter;
    use crate::types::{ChangePosition, GraphNode, Position};

    /// Create a test span
    fn make_vertex(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode::new(
            NodeId::new(change),
            ChangePosition::new(start),
            ChangePosition::new(end),
        )
    }

    /// Create a test change with content
    fn make_change(content: &[u8]) -> Change {
        let mut change = Change::empty(ChangeHeader::new("test"));
        change.contents = content.to_vec();
        change
    }

    /// Create a minimal alive graph with one node
    fn make_simple_graph(node: GraphNode<NodeId>) -> AliveGraph {
        let mut graph = AliveGraph::new();
        // Push dummy vertex first (required at index 0)
        graph.push_vertex(AliveVertex::DUMMY);
        // Push our actual node
        graph.push_vertex(AliveVertex::new(node));
        graph
    }

    /// Create a simple order result with one SCC containing one vertex
    /// Note: index 0 is DUMMY, so our vertex is at index 1
    fn make_simple_order() -> OrderResult {
        OrderResult {
            sccs: vec![vec![VertexId(1)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        }
    }

    // ------------------------------------------------------------------------
    // Basic Output Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_empty_graph() {
        let graph = AliveGraph::new();
        let order = OrderResult {
            sccs: Vec::new(),
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();
        let hash_fn = |_: NodeId| None;

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_output_single_vertex() {
        let content = b"Hello, world!";
        let node = make_vertex(1, 0, content.len() as u64);
        let graph = make_simple_graph(node);
        let order = make_simple_order();

        let changes = MemoryChangeStore::new();
        let change = make_change(content);
        let hash = change.hash().unwrap();
        changes.insert(hash, change);

        let hash_fn = |id: NodeId| {
            if id.get() == 1 {
                Some(hash)
            } else {
                None
            }
        };

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert_eq!(&buffer, content);
    }

    #[test]
    fn test_output_empty_vertex_skipped() {
        // Empty node (start == end)
        let node = make_vertex(1, 0, 0);
        let graph = make_simple_graph(node);
        let order = make_simple_order();

        let changes = MemoryChangeStore::new();
        let hash_fn = |_: NodeId| None;

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert!(buffer.is_empty()); // Empty node produces no output
    }

    // ------------------------------------------------------------------------
    // Cyclic Conflict Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_cyclic_conflict() {
        let content1 = b"Side A\n";
        let content2 = b"Side B\n";

        let vertex1 = make_vertex(1, 0, content1.len() as u64);
        let vertex2 = make_vertex(2, 0, content2.len() as u64);

        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        graph.push_vertex(AliveVertex::new(vertex1));
        graph.push_vertex(AliveVertex::new(vertex2));

        // Both vertices in same SCC = cyclic conflict (indices 1 and 2, since 0 is DUMMY)
        let order = OrderResult {
            sccs: vec![vec![VertexId(1), VertexId(2)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 1,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();

        let change1 = make_change(content1);
        let hash1 = change1.hash().unwrap();
        changes.insert(hash1, change1);

        let change2 = make_change(content2);
        let hash2 = change2.hash().unwrap();
        changes.insert(hash2, change2);

        let hash_fn = |id: NodeId| match id.get() {
            1 => Some(hash1),
            2 => Some(hash2),
            _ => None,
        };

        let mut buffer = Vec::new();
        let has_conflicts;
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());

            // Check conflicts while writer is still alive
            has_conflicts = writer.has_conflicts();
        }

        let output = String::from_utf8(buffer).unwrap();

        // Should have conflict markers
        assert!(output.contains(">>>>>>>"));
        assert!(output.contains("======="));
        assert!(output.contains("<<<<<<<"));

        // Should have both sides
        assert!(output.contains("Side A"));
        assert!(output.contains("Side B"));

        // Should have recorded a conflict
        assert!(has_conflicts);
    }

    // ------------------------------------------------------------------------
    // Multiple SCC Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_multiple_sccs() {
        let content1 = b"First\n";
        let content2 = b"Second\n";

        let vertex1 = make_vertex(1, 0, content1.len() as u64);
        let vertex2 = make_vertex(2, 0, content2.len() as u64);

        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        graph.push_vertex(AliveVertex::new(vertex1));
        graph.push_vertex(AliveVertex::new(vertex2));

        // Two separate SCCs (no conflict) - indices 1 and 2 since 0 is DUMMY
        let order = OrderResult {
            sccs: vec![vec![VertexId(1)], vec![VertexId(2)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();

        let change1 = make_change(content1);
        let hash1 = change1.hash().unwrap();
        changes.insert(hash1, change1);

        let change2 = make_change(content2);
        let hash2 = change2.hash().unwrap();
        changes.insert(hash2, change2);

        let hash_fn = |id: NodeId| match id.get() {
            1 => Some(hash1),
            2 => Some(hash2),
            _ => None,
        };

        let mut buffer = Vec::new();
        let has_conflicts;
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());

            // Check conflicts while writer is still alive
            has_conflicts = writer.has_conflicts();
        }

        let output = String::from_utf8(buffer).unwrap();

        // Both contents should appear, no conflict markers
        assert!(output.contains("First"));
        assert!(output.contains("Second"));
        assert!(!output.contains(">>>>>>>"));

        // No conflicts
        assert!(!has_conflicts);
    }

    // ------------------------------------------------------------------------
    // Zombie Conflict Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_zombie_vertex() {
        let content = b"Zombie content\n";
        let node = make_vertex(1, 0, content.len() as u64);

        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        let mut alive_vertex = AliveVertex::new(node);
        alive_vertex.mark_zombie(); // Mark as zombie
        graph.push_vertex(alive_vertex);

        let order = make_simple_order();

        let changes = MemoryChangeStore::new();
        let change = make_change(content);
        let hash = change.hash().unwrap();
        changes.insert(hash, change);

        let hash_fn = |id: NodeId| {
            if id.get() == 1 {
                Some(hash)
            } else {
                None
            }
        };

        let mut buffer = Vec::new();
        let has_conflicts;
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());

            // Check conflicts while writer is still alive
            has_conflicts = writer.has_conflicts();
        }

        let output = String::from_utf8(buffer).unwrap();

        // Should have zombie markers
        assert!(output.contains(">>>>>>>"));
        assert!(output.contains("<<<<<<<"));
        assert!(output.contains("Zombie content"));

        // Should have recorded a zombie conflict
        assert!(has_conflicts);
    }

    // ------------------------------------------------------------------------
    // Empty SCC Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_skips_empty_scc() {
        let content = b"Content\n";
        let node = make_vertex(1, 0, content.len() as u64);
        let graph = make_simple_graph(node);

        // Order with an empty SCC (shouldn't happen, but handle gracefully)
        let order = OrderResult {
            sccs: vec![vec![], vec![VertexId(1)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();
        let change = make_change(content);
        let hash = change.hash().unwrap();
        changes.insert(hash, change);

        let hash_fn = |id: NodeId| {
            if id.get() == 1 {
                Some(hash)
            } else {
                None
            }
        };

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert_eq!(&buffer, content);
    }

    // ------------------------------------------------------------------------
    // Missing Span Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_skips_missing_vertex() {
        let graph = AliveGraph::new(); // Empty graph

        // Order references a span that doesn't exist
        let order = OrderResult {
            sccs: vec![vec![VertexId(999)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();
        let hash_fn = |_: NodeId| None;

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert!(buffer.is_empty()); // Missing span produces no output
    }

    // ------------------------------------------------------------------------
    // Fork-Resolved Output Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_fork_resolved_output_writes_merged_skips_rest() {
        let content_merged = b"merged result\n";

        let vertex1 = make_vertex(1, 0, 5);
        let vertex2 = make_vertex(2, 0, 5);

        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        graph.push_vertex(AliveVertex::new(vertex1));
        graph.push_vertex(AliveVertex::new(vertex2));

        // Two single-vertex SCCs (fork children, no cycle)
        let order = OrderResult {
            sccs: vec![vec![VertexId(1)], vec![VertexId(2)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        // Simulate fork resolution
        let mut resolved = ResolvedConflicts::new();
        resolved.insert_merged(VertexId::new(1), content_merged.to_vec());
        resolved.insert_skip(VertexId::new(2));

        let changes = MemoryChangeStore::new();
        let hash_fn = |_: NodeId| None;

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content_resolved(
                &changes,
                hash_fn,
                &graph,
                &order,
                &mut writer,
                &resolved,
            );
            assert!(result.is_ok());
        }
        // Should contain only merged content, not the original vertices
        assert_eq!(&buffer, content_merged);
    }

    #[test]
    fn test_fork_resolved_empty_resolved_falls_through() {
        // With an empty ResolvedConflicts, single-vertex SCCs output normally
        let content = b"hello world";
        let vertex1 = make_vertex(1, 0, content.len() as u64);

        let graph = make_simple_graph(vertex1);
        let order = make_simple_order();
        let resolved = ResolvedConflicts::new();

        let changes = MemoryChangeStore::new();
        let change = make_change(content);
        let hash = change.hash().unwrap();
        changes.insert(hash, change);

        let hash_fn = |id: NodeId| {
            if id.get() == 1 {
                Some(hash)
            } else {
                None
            }
        };

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content_resolved(
                &changes,
                hash_fn,
                &graph,
                &order,
                &mut writer,
                &resolved,
            );
            assert!(result.is_ok());
        }
        assert_eq!(&buffer, content);
    }
}
