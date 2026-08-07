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

    // Deferred bypass attachments: when a vertex has both direct alive
    // children AND bypass children (live successors discovered through a
    // dead chain), we attach the bypass children to the LAST direct
    // child rather than to this vertex.  Since that direct child is
    // still on the stack and hasn't been processed yet, we record the
    // attachment here and apply it when the direct child pops.
    let mut pending_bypass: HashMap<VertexId, Vec<VertexId>> = HashMap::new();

    while let Some(vid) = stack.pop() {
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
        // The byte-graph alone does not encode whether a bypass-child
        // (e.g. a replacement of a later line in the dead chain) is
        // concurrent with a direct child (e.g. a replacement of an
        // earlier line).  In practice, when both exist, the bypass child
        // sits *after* the direct child in the linearised output — the
        // direct child is the FIRST replacement off this vertex, and the
        // bypass surfaces a deeper replacement reachable through the
        // dead chain that the direct child also leads into.
        //
        // After processing all edges we either:
        //   * attach bypass children directly when there is no direct
        //     alive child, or
        //   * attach them to the last direct alive child so they appear
        //     downstream rather than as a concurrent fork.
        let mut children_to_add: Vec<(Option<SerializedGraphEdge>, VertexId)> = Vec::new();
        let mut bypass_children: Vec<VertexId> = Vec::new();

        for edge in forward_edges {
            result.edges_traversed += 1;

            let dest_pos = edge.dest;

            // First resolve the position to an actual span using find_block.
            // This handles the case where position 9 could refer to either an
            // inode span V[9:9] or a content span V[9:23].
            let resolved_vertex = match txn.find_block(dest_pos) {
                Ok(v) => v,
                Err(_) => continue, // Position doesn't resolve to a span
            };

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
                let dest_alive = options.is_vertex_alive(txn, resolved_vertex)?;

                if !dest_alive {
                    let successors = walk_through_dead(
                        txn,
                        &options,
                        node,
                        resolved_vertex,
                        &mut stack,
                        &mut cache,
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
                    if !options.is_vertex_alive(txn, resolved_vertex)? {
                        // Vertex was deleted at the target state.  Skip
                        // emitting it, but walk THROUGH it to find live
                        // successors so we don't lose unrelated downstream
                        // content (no pseudo-edges in the additive model).
                        let successors = walk_through_dead(
                            txn,
                            &options,
                            node,
                            resolved_vertex,
                            &mut stack,
                            &mut cache,
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
                    let successors = walk_through_dead(
                        txn,
                        &options,
                        node,
                        resolved_vertex,
                        &mut stack,
                        &mut cache,
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

        // Place bypass children.
        //
        // If this vertex has at least one direct alive child, defer the
        // bypass children so they attach to the LAST direct child when
        // it's processed — they sit downstream of it in the linearised
        // output, not as concurrent siblings. If there is no direct
        // alive child, attach them directly so they appear after this
        // vertex.
        if !bypass_children.is_empty() {
            // In the insert-path graph, bypass successors reached through
            // dead chains belong after the downstream surviving child, not
            // alongside the first direct child. Attaching them to the
            // last direct child preserves linear file order when a change
            // both replaces an earlier line and carries through to later
            // descendants in the same chain.
            let direct_child =
                children_to_add
                    .iter()
                    .rev()
                    .find_map(|(_, v)| if !v.is_dummy() { Some(*v) } else { None });

            if let Some(direct) = direct_child {
                pending_bypass
                    .entry(direct)
                    .or_default()
                    .extend(bypass_children.iter().copied());
            } else {
                for succ_vid in &bypass_children {
                    if !children_to_add.iter().any(|(_, v)| v == succ_vid) {
                        children_to_add.push((None, *succ_vid));
                    }
                }
            }
        }

        // Apply any deferred bypass attachments for THIS vertex.
        if let Some(deferred) = pending_bypass.remove(&vid) {
            for succ_vid in deferred {
                if !children_to_add.iter().any(|(_, v)| *v == succ_vid) {
                    children_to_add.push((None, succ_vid));
                }
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

    Ok(result)
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
fn walk_through_dead<T: GraphTxnT>(
    txn: &T,
    options: &RetrieveOptions,
    owner_parent: GraphNode<NodeId>,
    dead_vertex: GraphNode<NodeId>,
    stack: &mut Vec<VertexId>,
    cache: &mut std::collections::HashMap<GraphNode<NodeId>, VertexId>,
    result: &mut RetrieveResult,
) -> Result<Vec<VertexId>, PristineError> {
    use std::collections::HashSet;
    let mut live_successors: Vec<VertexId> = Vec::new();

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

    while let Some(current) = queue.pop() {
        // Walk current's forward edges (include deleted so we can chain
        // through multiple dead vertices in a row).
        let edges = txn.iter_forward(current, true)?;

        for edge in edges {
            let next_pos = edge.dest;
            let next_vertex = match txn.find_block(next_pos) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if seen.contains(&next_vertex) {
                continue;
            }
            seen.insert(next_vertex);

            // Check change filter
            if !options.passes_filter(next_vertex.change) {
                continue;
            }

            // Is this vertex alive in our view?
            let alive = options.is_vertex_alive(txn, next_vertex)?;

            if alive {
                // Live successor. Only surface it as a bypass child when
                // there is no OTHER alive parent outside the dead chain.
                // Ancestors of `owner_parent` in the already-built alive
                // graph do not count as alternates; they are the same
                // linear chain we are currently bypassing for.
                let has_alive_alt_parent = {
                    let parents = txn.iter_parents(next_vertex, true)?;
                    parents.iter().any(|p| {
                        if !options.passes_filter(p.introduced_by) {
                            return false;
                        }
                        match p.kind {
                            crate::types::ParentEdgeKind::Block
                            | crate::types::ParentEdgeKind::Folder => {
                                let source_vertex = match txn.find_block_end(p.dest) {
                                    Ok(v) => v,
                                    Err(_) => return false,
                                };
                                let source_alive =
                                    match options.is_vertex_alive(txn, source_vertex) {
                                        Ok(alive) => alive,
                                        Err(_) => return false,
                                    };
                                if !source_alive {
                                    return false;
                                }
                                if source_vertex == owner_parent {
                                    return false;
                                }
                                if let (Some(&source_vid), Some(&owner_vid)) =
                                    (cache.get(&source_vertex), cache.get(&owner_parent))
                                {
                                    if alive_graph_reaches(&result.graph, source_vid, owner_vid) {
                                        return false;
                                    }
                                }
                                !dead_visited.contains(&source_vertex)
                            }
                            _ => false,
                        }
                    })
                };

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

                if !has_alive_alt_parent && !live_successors.contains(&vid) {
                    let shadowed_by_existing =
                        live_successors.iter().copied().any(|existing_vid| {
                            let existing_node = result.graph.get_vertex(existing_vid).node;
                            visible_chain_reaches(txn, options, existing_node, next_vertex)
                        });

                    if !shadowed_by_existing {
                        live_successors.retain(|existing_vid| {
                            let existing_node = result.graph.get_vertex(*existing_vid).node;
                            !visible_chain_reaches(txn, options, next_vertex, existing_node)
                        });
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
                queue.push(next_vertex);
            }
        }
    }

    Ok(live_successors)
}

fn visible_chain_reaches<T: GraphTxnT>(
    txn: &T,
    options: &RetrieveOptions,
    start: GraphNode<NodeId>,
    target: GraphNode<NodeId>,
) -> bool {
    if start == target {
        return true;
    }

    let mut stack = vec![start];
    let mut seen = std::collections::HashSet::new();

    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }

        let edges = match txn.iter_forward(current, true) {
            Ok(edges) => edges,
            Err(_) => continue,
        };

        for edge in edges {
            if edge.kind.is_pseudo() {
                continue;
            }

            let next = match txn.find_block(edge.dest) {
                Ok(next) => next,
                Err(_) => continue,
            };

            if !options.passes_filter(next.change) {
                continue;
            }

            if next == target {
                return true;
            }

            stack.push(next);
        }
    }

    false
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
