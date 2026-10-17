//! Graph retrieval from the pristine database
//!
//! This module handles building an [`AliveGraph`] by traversing the repository
//! graph starting from a file's inode position. It discovers all alive (non-deleted)
//! vertices that make up the file's content.
//!
//! # Overview
//!
//! Graph retrieval is the first step in outputting a file's content:
//!
//! 1. Start from the file's inode position (stored in the tree)
//! 2. Follow forward edges to discover content vertices
//! 3. Skip deleted vertices (unless `include_deleted` is set)
//! 4. Mark zombie vertices (deleted but with live connections)
//! 5. Build the `AliveGraph` with all discovered vertices and edges
//!
//! # Algorithm
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Retrieve Algorithm                               │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  1. Initialize                        2. Process Stack                  │
//! │  ┌─────────────────────────┐         ┌─────────────────────────┐        │
//! │  │ - Create empty graph    │         │ while stack not empty:  │        │
//! │  │ - Add DUMMY at index 0  │ ──────▶ │   - Pop span          │        │
//! │  │ - Add root at index 1   │         │   - Get adjacent edges  │        │
//! │  │ - Push root to stack    │         │   - For each forward:   │        │
//! │  │ - Create position cache │         │     - Find/add span   │        │
//! │  └─────────────────────────┘         │     - Add to children   │        │
//! │                                      │     - Push if new       │        │
//! │                                      └─────────────────────────┘        │
//! │                                                                         │
//! │  3. Span Classification                                               │
//! │  ┌─────────────────────────────────────────────────────────────┐        │
//! │  │ For each position:                                          │        │
//! │  │ - Find block containing position                            │        │
//! │  │ - Check if alive (has non-deleted parent edges)             │        │
//! │  │ - Check if zombie (deleted but has live parents)            │        │
//! │  │ - Skip if not alive and not include_deleted                 │        │
//! │  └─────────────────────────────────────────────────────────────┘        │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Performance
//!
//! The retrieval algorithm is O(V + E) where V is the number of vertices and
//! E is the number of edges in the file's subgraph. A position cache prevents
//! revisiting the same positions.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::alive::{retrieve_graph, RetrieveOptions};
//!
//! // Basic retrieval (exclude deleted content)
//! let graph = retrieve_graph(&txn, file_position, RetrieveOptions::default())?;
//!
//! // Include deleted content (for showing conflicts)
//! let options = RetrieveOptions::new().include_deleted(true);
//! let graph_with_deleted = retrieve_graph(&txn, file_position, options)?;
//! ```

mod classify;
mod options;
#[cfg(test)]
mod tests;

pub use options::{RetrieveOptions, RetrieveResult};

use classify::create_alive_vertex;

use super::graph::AliveGraph;
use super::vertex::{AliveVertex, VertexId};
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{GraphNode, NodeId, Position, SerializedGraphEdge};
use std::collections::HashMap;

/// Retrieve the alive graph for a file starting from a position.
///
/// This function traverses the graph from the given starting position,
/// collecting all alive vertices and their edges into an `AliveGraph`.
///
/// # Arguments
///
/// * `txn` - The transaction providing graph access
/// * `start_pos` - Starting position (typically the file's inode position)
/// * `options` - Retrieval options
///
/// # Returns
///
/// A `RetrieveResult` containing the graph and statistics.
///
/// # Errors
///
/// Returns an error if database access fails.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::alive::{retrieve_graph, RetrieveOptions};
///
/// let result = retrieve_graph(&txn, file_pos, RetrieveOptions::default())?;
/// println!("Retrieved {} vertices", result.graph.len_vertices());
/// ```
pub fn retrieve_graph<T: GraphTxnT>(
    txn: &T,
    start_pos: Position<NodeId>,
    options: RetrieveOptions,
) -> Result<RetrieveResult, PristineError> {
    let mut result = RetrieveResult::new(AliveGraph::new());

    // Span cache to avoid revisiting - keyed by the actual span, not position.
    // This is important because a single position (e.g., position 9) might refer to
    // different vertices: an empty inode span V[9:9] or a content span V[9:23].
    // Using the resolved span as the key ensures we visit each unique span once.
    let mut cache: HashMap<GraphNode<NodeId>, VertexId> = HashMap::new();
    // Per-retrieval aliveness memo. `is_vertex_alive` iterates the vertex's
    // parent edges and resolves causal dominance between them; on a heavily
    // edited file the same vertices are re-checked across the main traversal
    // and every `walk_through_dead` walk, which is the dominant repeated work.
    // Aliveness is a pure function of (txn, options, vertex), so one memo per
    // retrieval removes the repetition without changing any verdict.
    let mut alive_memo: HashMap<GraphNode<NodeId>, bool> = HashMap::new();
    // Per-retrieval reachability memo for the dead-walk's chain comparisons.
    let mut chain_memo: HashMap<(GraphNode<NodeId>, GraphNode<NodeId>), bool> = HashMap::new();
    // Shared causal-dominance memo across every aliveness check in this walk.
    let mut dep_memo: HashMap<(NodeId, NodeId), bool> = HashMap::new();
    let mut dead_contexts = std::collections::HashSet::new();
    // A liveness query needs only reachability to any live bytes, independent
    // of the owner-specific ordering of bypass edges. Share dead visits across
    // root edges so overlapping deleted histories are scanned once.
    let mut liveness_dead_seen = std::collections::HashSet::new();
    // Negative reachability proofs are independent of the bypass owner.
    // Reuse only fully explored subgraphs with no live successor at all;
    // positive walks retain their owner-specific ordering and attachment.
    let mut empty_dead_subgraphs = std::collections::HashSet::new();

    // Add dummy span at index 0
    result.graph.push_vertex(AliveVertex::DUMMY);
    cache.insert(GraphNode::BOTTOM, VertexId::DUMMY);

    // Add the root span (inode span) at index 1
    // But only if it passes the change filter
    let root_vertex = start_pos.inode_node();

    // Check if root span passes the change filter
    if !options.passes_filter(start_pos.change) {
        // Root span is filtered out - return empty graph
        result.was_filtered = options.has_filter();
        return Ok(result);
    }

    // Track whether a change filter is active so callers can distinguish
    // "genuinely empty file" from "file belongs to a different view".
    result.was_filtered = options.has_filter();

    let root_alive = AliveVertex::new(root_vertex);
    result.graph.push_vertex(root_alive);
    cache.insert(root_vertex, VertexId::new(1));

    // DFS traversal stack
    let mut stack = vec![VertexId::new(1)];

    // Determine whether iter_forward should include deleted edges.
    // When a change filter is active we need to see deleted edges so we can
    // decide whether the deletion "has happened" from our view's perspective.
    let include_deleted = options.include_deleted_edges();

    while let Some(vid) = stack.pop() {
        // Liveness fast path (review E4): the moment the traversal holds
        // one alive non-empty vertex, the caller's "any alive bytes?"
        // question is answered; walking the rest of a long-history file is
        // wasted work. The option is opt-in and the graph is partial.
        if options.stop_at_first_content && result.graph.total_bytes() > 0 {
            break;
        }
        // Check span limit
        if let Some(max) = options.max_vertices {
            if result.graph.len_vertices() >= max {
                result.truncated = true;
                break;
            }
        }

        // Mark where this span's children start.
        // We need to set this on the SPECIFIC span we're processing (vid),
        // not just the "last" span, because we may have pushed new vertices
        // during previous iterations.
        let children_start = result.graph.len_children();
        {
            let current_vertex = result.graph.get_vertex_mut(vid).unwrap();
            current_vertex.children = children_start;
        }

        // Get the node to traverse
        let node = result.graph.get_vertex(vid).node;

        // Get typed forward edges — parent edges are excluded at the type
        // level, so no manual `if PARENT { continue }` guard is needed.
        let forward_edges = txn.iter_forward(node, include_deleted)?;

        // Collect direct children (via alive edges to alive vertices) and
        // bypass children (live successors discovered by walking through
        // adjacent dead vertices) separately.
        //
        let mut children_to_add: Vec<(Option<SerializedGraphEdge>, VertexId)> = Vec::new();
        let mut bypass_children: Vec<VertexId> = Vec::new();

        for edge in forward_edges {
            if options.stop_at_first_content && result.graph.total_bytes() > 0 {
                break;
            }
            result.edges_traversed += 1;

            let dest_pos = edge.dest;

            // First resolve the position to an actual span using find_block.
            // This handles the case where position 9 could refer to either an
            // inode span V[9:9] or a content span V[9:23].
            let resolved_vertex = txn.find_block(dest_pos)?;

            // Check if this span passes the change filter.
            // This is the key mechanism for state-based content retrieval:
            // only include vertices from changes that existed at the target state.
            if !options.passes_filter(resolved_vertex.change) {
                continue; // Span is from a change not in the filter set
            }

            // Single-edge alive check using the typed EdgeKind.
            // With no filter, deleted edges are skipped.
            // With a filter, a deleted edge is skipped only if its introducing
            // change is IN the filter (meaning the deletion has happened from
            // our view's perspective).
            if !options.is_edge_alive(&edge) {
                // The edge is dead from this view's perspective.
                // Check if the destination vertex itself is also dead
                // — if so, we need to walk THROUGH it to find live
                // descendants (no PSEUDO reconnection edges in the
                // additive model).  If the destination vertex is alive
                // through some OTHER edge, the normal traversal will
                // pick it up via that path.
                let dest_alive = memo_vertex_alive(
                    &options,
                    txn,
                    resolved_vertex,
                    &mut alive_memo,
                    &mut dep_memo,
                )?;

                if !dest_alive {
                    dead_contexts.insert((vid, resolved_vertex));
                    let successors = walk_through_dead(
                        txn,
                        &options,
                        node,
                        resolved_vertex,
                        &mut stack,
                        &mut cache,
                        &mut alive_memo,
                        &mut chain_memo,
                        &mut dep_memo,
                        &mut liveness_dead_seen,
                        &mut empty_dead_subgraphs,
                        &mut result,
                    )?;
                    // Collect bypass children; placement decided after
                    // all forward edges have been seen.
                    for succ_vid in successors {
                        if !bypass_children.contains(&succ_vid) {
                            bypass_children.push(succ_vid);
                        }
                    }
                }
                continue;
            }

            // Check if we've already visited this resolved span
            let dest_vid = if let Some(&existing) = cache.get(&resolved_vertex) {
                existing
            } else {
                result.positions_visited += 1;

                // When we have a change filter (or deletions are final) and are
                // doing state-based retrieval, we need to check if the vertex is
                // alive at the target state by examining all its parent edges
                // (not just the edge we followed).
                // Otherwise, use the normal create_alive_vertex check.
                let alive_vertex = if options.deletion_aware() {
                    // Full vertex aliveness check using typed parent iteration
                    if !memo_vertex_alive(
                        &options,
                        txn,
                        resolved_vertex,
                        &mut alive_memo,
                        &mut dep_memo,
                    )? {
                        // Vertex was deleted at the target state.  Skip
                        // emitting it, but walk THROUGH it to find live
                        // successors so we don't lose unrelated downstream
                        // content (no pseudo-edges in the additive model).
                        dead_contexts.insert((vid, resolved_vertex));
                        let successors = walk_through_dead(
                            txn,
                            &options,
                            node,
                            resolved_vertex,
                            &mut stack,
                            &mut cache,
                            &mut alive_memo,
                            &mut chain_memo,
                            &mut dep_memo,
                            &mut liveness_dead_seen,
                            &mut empty_dead_subgraphs,
                            &mut result,
                        )?;
                        for succ_vid in successors {
                            if !bypass_children.contains(&succ_vid) {
                                bypass_children.push(succ_vid);
                            }
                        }
                        continue;
                    }
                    AliveVertex::new(resolved_vertex)
                } else if let Some(av) = create_alive_vertex(txn, resolved_vertex)? {
                    av
                } else {
                    // Span is not alive, skip — but still walk through
                    // to find live successors.
                    dead_contexts.insert((vid, resolved_vertex));
                    let successors = walk_through_dead(
                        txn,
                        &options,
                        node,
                        resolved_vertex,
                        &mut stack,
                        &mut cache,
                        &mut alive_memo,
                        &mut chain_memo,
                        &mut dep_memo,
                        &mut liveness_dead_seen,
                        &mut empty_dead_subgraphs,
                        &mut result,
                    )?;
                    for succ_vid in successors {
                        if !bypass_children.contains(&succ_vid) {
                            bypass_children.push(succ_vid);
                        }
                    }
                    continue;
                };

                let new_id = result.graph.push_vertex(alive_vertex);
                cache.insert(resolved_vertex, new_id);
                stack.push(new_id);
                new_id
            };

            // Convert the typed ForwardEdge back to a SerializedGraphEdge
            // for storage in the AliveGraph children list, which uses the
            // wire format.
            let serialized =
                SerializedGraphEdge::new(edge.kind.to_flags(), edge.dest, edge.introduced_by);

            // Dedupe: if another already-collected child points to the
            // same vid (because the same destination is reachable via
            // multiple alive Block edges from different changes), don't
            // add a duplicate.  Multiple parallel paths to the same
            // vertex are a property of the additive edge model — fork
            // detection only cares about distinct successors.
            if !children_to_add
                .iter()
                .any(|(_, existing_vid)| *existing_vid == dest_vid)
            {
                children_to_add.push((Some(serialized), dest_vid));
            }
        }

        // Contract a dead chain at the predecessor from which it was reached.
        // A bypass successor may precede a direct child, or be concurrent with
        // it. Attaching it to an arbitrary direct child invents an ordering
        // edge (and can create a cycle). It can also lose the attachment if
        // that child has already been processed. Keep the canonical reachability
        // relation here; the output graph handles redundant paths and forks.
        for successor in bypass_children {
            if !children_to_add.iter().any(|(_, child)| *child == successor) {
                children_to_add.push((None, successor));
            }
        }

        // Add sentinel at end of children
        children_to_add.push((None, VertexId::DUMMY));

        // Now add all children and update the count for the span we're processing
        for (edge, child_vid) in children_to_add {
            result.graph.push_child(edge, child_vid);
        }

        // Update children count for the span we processed
        let children_end = result.graph.len_children();
        {
            let current_vertex = result.graph.get_vertex_mut(vid).unwrap();
            current_vertex.n_children = children_end - current_vertex.children;
        }
    }

    // The opt-in boolean query does not need to reconstruct line ordering.
    if options.stop_at_first_content {
        return Ok(result);
    }

    // Adjacent independent replacements leave each new span anchored to
    // deleted old context. For old A -> B, replacing A with X and B with Y
    // records X -> old B and old A -> Y. Contracting old spans alone loses
    // X -> Y. Recover that ordering only with both replacement witnesses:
    // Y was inserted where B was deleted, and X descends from the replacement
    // of B's old predecessor A. Unrelated same-anchor inserts remain forks.
    // Resolve after traversal so discovery/edge order cannot hide a claimant.
    let mut replacement_cache = HashMap::new();
    for (owner_vid, dead) in dead_contexts {
        let owner = result.graph.get_vertex(owner_vid).node;
        for (predecessor, replacement) in
            replacement_roots(txn, &options, dead, &mut replacement_cache)?
        {
            if memo_vertex_alive(&options, txn, predecessor, &mut alive_memo, &mut dep_memo)? {
                continue;
            }
            let Some(&replacement_vid) = cache.get(&replacement) else {
                continue;
            };
            if owner == replacement
                || memo_visible_chain_reaches(txn, &options, replacement, owner, &mut chain_memo)?
            {
                continue;
            }
            let mut replaces_predecessor = false;
            for (_, predecessor_replacement) in
                replacement_roots(txn, &options, predecessor, &mut replacement_cache)?
            {
                if memo_visible_chain_reaches(
                    txn,
                    &options,
                    predecessor_replacement,
                    owner,
                    &mut chain_memo,
                )? {
                    replaces_predecessor = true;
                    break;
                }
            }
            if replaces_predecessor
                && !result
                    .graph
                    .children(owner_vid)
                    .any(|(_, child)| *child == replacement_vid)
            {
                result
                    .graph
                    .get_vertex_mut(owner_vid)
                    .unwrap()
                    .extra
                    .push((None, replacement_vid));
            }
        }
    }
    Ok(result)
}

/// Pair a deleted incoming binding with new content inserted by the same
/// change at that exact predecessor. These are canonical graph witnesses,
/// independent of path indexes, local insertion order, and Git metadata.
type ReplacementRoot = (GraphNode<NodeId>, GraphNode<NodeId>);
type ReplacementRootsMemo = HashMap<GraphNode<NodeId>, Vec<ReplacementRoot>>;

fn replacement_roots<T: GraphTxnT>(
    txn: &T,
    options: &RetrieveOptions,
    dead: GraphNode<NodeId>,
    memo: &mut ReplacementRootsMemo,
) -> Result<Vec<ReplacementRoot>, PristineError> {
    if let Some(roots) = memo.get(&dead) {
        return Ok(roots.clone());
    }
    let mut roots = Vec::new();
    for parent in txn.iter_parents(dead, true)? {
        if parent.kind != crate::types::ParentEdgeKind::BlockDeleted
            || !options.passes_filter(parent.introduced_by)
        {
            continue;
        }
        let predecessor = txn.find_block_end(parent.dest)?;
        if !options.passes_filter(predecessor.change) {
            continue;
        }
        for edge in txn.iter_forward(predecessor, false)? {
            if edge.introduced_by != parent.introduced_by || !options.is_edge_alive(&edge) {
                continue;
            }
            let replacement = txn.find_block(edge.dest)?;
            if replacement.change == parent.introduced_by && replacement != dead {
                roots.push((predecessor, replacement));
            }
        }
    }
    memo.insert(dead, roots.clone());
    Ok(roots)
}

/// Walk through a dead vertex's forward edges to find live successors.
///
/// In the additive edge model there are no PSEUDO reconnection edges,
/// so the alive-graph traversal must dynamically skip over dead vertices
/// to find live descendants.  This walks the dead vertex's BLOCK edges
/// (including deleted ones, since we may need to skip multiple dead
/// vertices in a row) and pushes any live destinations onto the stack.
///
/// Dead vertices are not added to the alive graph — they don't appear in
/// output.  Only their live descendants are recorded.
/// Returns the live successor `VertexId`s reachable through this dead
/// vertex (and any chain of dead vertices beyond it).  Caller is
/// responsible for adding those successors to its `children_to_add`
/// list so they appear as proper graph children of the upstream alive
/// parent.
/// Memoized vertex-aliveness lookup for one `retrieve_graph` call.
fn memo_vertex_alive<T: GraphTxnT>(
    options: &RetrieveOptions,
    txn: &T,
    vertex: GraphNode<NodeId>,
    memo: &mut std::collections::HashMap<GraphNode<NodeId>, bool>,
    dep_memo: &mut std::collections::HashMap<(NodeId, NodeId), bool>,
) -> Result<bool, PristineError> {
    if let Some(&alive) = memo.get(&vertex) {
        return Ok(alive);
    }
    let alive = options.is_vertex_alive_with_memo(txn, vertex, dep_memo)?;
    memo.insert(vertex, alive);
    Ok(alive)
}

#[allow(clippy::too_many_arguments)] // memoised traversal; each param is a distinct cache
fn walk_through_dead<T: GraphTxnT>(
    txn: &T,
    options: &RetrieveOptions,
    owner_parent: GraphNode<NodeId>,
    dead_vertex: GraphNode<NodeId>,
    stack: &mut Vec<VertexId>,
    cache: &mut std::collections::HashMap<GraphNode<NodeId>, VertexId>,
    alive_memo: &mut std::collections::HashMap<GraphNode<NodeId>, bool>,
    chain_memo: &mut std::collections::HashMap<(GraphNode<NodeId>, GraphNode<NodeId>), bool>,
    dep_memo: &mut std::collections::HashMap<(NodeId, NodeId), bool>,
    liveness_dead_seen: &mut std::collections::HashSet<GraphNode<NodeId>>,
    empty_dead_subgraphs: &mut std::collections::HashSet<GraphNode<NodeId>>,
    result: &mut RetrieveResult,
) -> Result<Vec<VertexId>, PristineError> {
    use std::collections::HashSet;
    let mut live_successors: Vec<VertexId> = Vec::new();
    if empty_dead_subgraphs.contains(&dead_vertex)
        || (options.stop_at_first_content && !liveness_dead_seen.insert(dead_vertex))
    {
        return Ok(live_successors);
    }

    // BFS through dead vertices, recording live ones we encounter.
    //
    // We track three sets separately:
    //   * `dead_visited` — dead vertices we have walked through.  This is
    //     the set that an "alive outsider" check should consult: a dead
    //     vertex `D` is claimed by some alive parent `P` only when `P` is
    //     NOT in this set (i.e. `P` is not part of the dead chain).
    //   * `alive_found`  — alive vertices reached during this walk.  We
    //     remember them only to avoid pushing the same one to the graph
    //     twice, but they must NOT mask the alive-outsider check.
    //   * `seen`         — union, used purely to skip re-visiting the
    //     same vertex during BFS.
    let mut dead_visited: HashSet<GraphNode<NodeId>> = HashSet::new();
    let mut seen: HashSet<GraphNode<NodeId>> = HashSet::new();
    let mut queue: Vec<GraphNode<NodeId>> = vec![dead_vertex];
    seen.insert(dead_vertex);
    dead_visited.insert(dead_vertex);
    let mut saw_live_successor = false;

    while let Some(current) = queue.pop() {
        // Walk current's forward edges (include deleted so we can chain
        // through multiple dead vertices in a row).
        let edges = txn.iter_forward(current, true)?;

        for edge in edges {
            let next_pos = edge.dest;
            let next_vertex = txn.find_block(next_pos)?;

            if seen.contains(&next_vertex)
                || empty_dead_subgraphs.contains(&next_vertex)
                || (options.stop_at_first_content && liveness_dead_seen.contains(&next_vertex))
            {
                continue;
            }
            seen.insert(next_vertex);

            // Check change filter
            if !options.passes_filter(next_vertex.change) {
                continue;
            }

            // Is this vertex alive in our view?
            let alive = memo_vertex_alive(options, txn, next_vertex, alive_memo, dep_memo)?;

            if alive {
                saw_live_successor = true;
                // Live successor. Only surface it as a bypass child when
                // there is no OTHER alive parent outside the dead chain.
                // Ancestors of `owner_parent` in the already-built alive
                // graph do not count as alternates; they are the same
                // linear chain we are currently bypassing for.
                let mut has_alive_alt_parent = false;
                for parent in txn.iter_parents(next_vertex, true)? {
                    if !options.passes_filter(parent.introduced_by) {
                        continue;
                    }

                    match parent.kind {
                        crate::types::ParentEdgeKind::Block
                        | crate::types::ParentEdgeKind::Folder => {
                            let source_vertex = txn.find_block_end(parent.dest)?;
                            if !memo_vertex_alive(
                                options,
                                txn,
                                source_vertex,
                                alive_memo,
                                dep_memo,
                            )? {
                                continue;
                            }
                            if source_vertex == owner_parent {
                                continue;
                            }
                            if let (Some(&source_vid), Some(&owner_vid)) =
                                (cache.get(&source_vertex), cache.get(&owner_parent))
                            {
                                if alive_graph_reaches(&result.graph, source_vid, owner_vid) {
                                    continue;
                                }
                            }
                            if !dead_visited.contains(&source_vertex) {
                                has_alive_alt_parent = true;
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                // Add to alive graph (so the normal traversal can find it
                // via the alternate path).  Push onto stack so its
                // descendants are discovered.
                let vid = if let Some(&existing) = cache.get(&next_vertex) {
                    existing
                } else {
                    let av = AliveVertex::new(next_vertex);
                    let new_id = result.graph.push_vertex(av);
                    cache.insert(next_vertex, new_id);
                    stack.push(new_id);
                    new_id
                };

                if options.stop_at_first_content && result.graph.total_bytes() > 0 {
                    return Ok(vec![vid]);
                }

                if !has_alive_alt_parent && !live_successors.contains(&vid) {
                    let mut shadowed_by_existing = false;
                    for existing_vid in live_successors.iter().copied() {
                        let existing_node = result.graph.get_vertex(existing_vid).node;
                        if memo_visible_chain_reaches(
                            txn,
                            options,
                            existing_node,
                            next_vertex,
                            chain_memo,
                        )? {
                            shadowed_by_existing = true;
                            break;
                        }
                    }

                    if !shadowed_by_existing {
                        let mut retained = Vec::with_capacity(live_successors.len());
                        for existing_vid in live_successors.drain(..) {
                            let existing_node = result.graph.get_vertex(existing_vid).node;
                            if !memo_visible_chain_reaches(
                                txn,
                                options,
                                next_vertex,
                                existing_node,
                                chain_memo,
                            )? {
                                retained.push(existing_vid);
                            }
                        }
                        live_successors = retained;
                        live_successors.push(vid);
                    }
                }
            } else {
                // The vertex is dead.  Before continuing to walk through
                // it, check whether it has an alive parent edge from a
                // vertex OUTSIDE our dead-walk set.  If so, that alive
                // parent's own forward walk will discover this dead
                // chain's downstream — we shouldn't also surface them as
                // bypass-children of the upstream caller (would create
                // diamond/duplicate paths that fork-detection misreads as
                // CRDT conflicts).
                // Still dead — keep walking through it.
                dead_visited.insert(next_vertex);
                if options.stop_at_first_content {
                    liveness_dead_seen.insert(next_vertex);
                }
                queue.push(next_vertex);
            }
        }
    }

    if !options.stop_at_first_content && !saw_live_successor {
        // Every outgoing route was exhausted under the same immutable
        // transaction/filter. This says nothing about positive bypass paths.
        empty_dead_subgraphs.extend(dead_visited);
    }
    Ok(live_successors)
}

/// Memoized `visible_chain_reaches` for one `retrieve_graph` call. The
/// reachability question is a pure function of (txn, options, start, target),
/// and the dead-walk asks the same pairs repeatedly; without the memo each
/// answer re-runs a full forward DFS.
fn memo_visible_chain_reaches<T: GraphTxnT>(
    txn: &T,
    options: &RetrieveOptions,
    start: GraphNode<NodeId>,
    target: GraphNode<NodeId>,
    memo: &mut std::collections::HashMap<(GraphNode<NodeId>, GraphNode<NodeId>), bool>,
) -> Result<bool, PristineError> {
    let key = (start, target);
    if let Some(&reaches) = memo.get(&key) {
        return Ok(reaches);
    }
    let reaches = visible_chain_reaches(txn, options, start, target)?;
    memo.insert(key, reaches);
    Ok(reaches)
}

fn visible_chain_reaches<T: GraphTxnT>(
    txn: &T,
    options: &RetrieveOptions,
    start: GraphNode<NodeId>,
    target: GraphNode<NodeId>,
) -> Result<bool, PristineError> {
    if start == target {
        return Ok(true);
    }

    let mut stack = vec![start];
    let mut seen = std::collections::HashSet::new();

    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }

        for edge in txn.iter_forward(current, true)? {
            if edge.kind.is_pseudo() || !options.passes_filter(edge.introduced_by) {
                continue;
            }

            let next = txn.find_block(edge.dest)?;

            if !options.passes_filter(next.change) {
                continue;
            }

            if next == target {
                return Ok(true);
            }

            stack.push(next);
        }
    }

    Ok(false)
}

fn alive_graph_reaches(graph: &AliveGraph, from: VertexId, target: VertexId) -> bool {
    if from == target {
        return true;
    }

    let mut stack = vec![from];
    let mut seen = std::collections::HashSet::new();

    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }

        for (_, child) in graph.children(current) {
            if child.is_dummy() {
                continue;
            }
            if *child == target {
                return true;
            }
            stack.push(*child);
        }
    }

    false
}
