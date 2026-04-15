//! Globalize a single built hunk into a graph operation.
//!
//! # Performance
//!
//! Content vertex discovery uses the `INODE_GRAPH` secondary index when the
//! transaction implements [`InodeGraphOps`].  This gives O(m) traversal
//! where m = edges for THIS file, instead of O(n) where n = all edges in
//! the repository.  See the [Performance at Scale] documentation for the
//! dual-index architecture.
//!
//! This module converts a local working-copy change (a [`BuiltHunk`]) into a
//! graph-compatible [`GraphOp<Option<Hash>>`].
//!
//! # Design
//!
//! After the upstream consolidation in `record_modified_file`, every hunk that
//! reaches this function belongs to exactly one of four operations:
//!
//! | Hunk kind | Condition | Graph operation |
//! |-----------|-----------|-----------------|
//! | `Insert`  | `old_start == 0` | **Prepend**: insert before first content |
//! | `Insert`  | `old_start >= old_lines` | **Append**: insert after last content |
//! | `Replace` | *(always)* | **Replace**: delete all old → insert new |
//! | `Delete`  | *(always)* | **Delete**: delete all old content |
//!
//! Middle insertions (`0 < old_start < old_lines`) are collapsed into a
//! single Replace by the upstream consolidation step. If one somehow
//! reaches here (e.g. a future code path bypasses the consolidation), we
//! return a clear error rather than silently duplicating content.
//!
//! Both `Replace` and `Delete` need the same "find every content vertex and
//! build deletion edges" work, so that logic lives in one place:
//! [`delete_all_content`].

use super::*;
use crate::change::Local;
use crate::pristine::InodeGraphOps;

// ───────────────────────────────────────────────────────────────────────────
// Public entry point
// ───────────────────────────────────────────────────────────────────────────

/// Globalize a single built hunk into a graph operation.
///
/// # Arguments
///
/// * `ctx`            – globalization context (content buffer, dependencies, txn)
/// * `built`          – the built hunk from the recording phase
/// * `inode`          – the file's stable identifier
/// * `inode_pos`      – the graph position of the file's inode vertex
/// * `content`        – the content bytes this hunk should insert (the hunk's
///   own slice for Insert, the full new file for Replace)
/// * `old_line_count` – number of lines in the old file (for insert-position
///   classification)
pub fn globalize_hunk<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    built: &BuiltHunk,
    inode: Inode,
    inode_pos: Position<NodeId>,
    content: &[u8],
    old_line_count: Option<usize>,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let local = built.local.clone();
    let encoding = built.encoding;

    match built.kind {
        BuiltHunkKind::Insert => globalize_insert(
            ctx,
            built,
            inode,
            inode_pos,
            content,
            old_line_count,
            local,
            encoding,
        ),
        BuiltHunkKind::Replace => {
            globalize_replace(ctx, inode, inode_pos, content, local, encoding)
        }
        BuiltHunkKind::Delete => globalize_delete(ctx, inode, inode_pos, local, encoding),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Insert: prepend / append (the only two safe positions)
// ───────────────────────────────────────────────────────────────────────────

/// Classify an Insert hunk and emit the corresponding graph operation.
///
/// Only two positions are supported:
///
/// * **Prepend** (`old_start == 0`): new content is wired between the inode
///   vertex and the first existing content vertex.
/// * **Append** (`old_start >= old_line_count`): new content is wired after
///   the last existing content vertex.
///
/// Any other position means the upstream consolidation was bypassed, and we
/// return an error instead of silently producing duplicate content.
#[allow(clippy::too_many_arguments)]
fn globalize_insert<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    built: &BuiltHunk,
    inode: Inode,
    inode_pos: Position<NodeId>,
    content: &[u8],
    old_line_count: Option<usize>,
    local: Local,
    encoding: Option<Encoding>,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let position = classify_insert(ctx.txn(), inode, inode_pos, built.old_start, old_line_count)?;

    let (predecessors, successors) = match position {
        InsertPosition::Prepend { first_content } => (vec![inode_pos], vec![first_content]),
        InsertPosition::Append { last_content_end } => (vec![last_content_end], vec![]),
    };

    let vertex = create_content_vertex(ctx, inode, inode_pos, predecessors, successors, content)?;

    Ok(GraphOp::Edit {
        change: Atom::Insertion(vertex),
        local,
        encoding,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Replace: delete all old content, insert full new content
// ───────────────────────────────────────────────────────────────────────────

/// Delete every existing content vertex and insert `content` (the full new
/// file) as a single vertex connected to the inode.
fn globalize_replace<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode: Inode,
    inode_pos: Position<NodeId>,
    content: &[u8],
    local: Local,
    encoding: Option<Encoding>,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let step = std::time::Instant::now();
    let content_vertices = find_content_vertices(ctx.txn(), inode, inode_pos)?;
    let find_ms = step.elapsed().as_millis();

    let step = std::time::Instant::now();
    let deletion_edges = match build_deletion_edges(ctx, &content_vertices) {
        Ok(edges) => edges,
        Err(e) => {
            // The INODE_GRAPH fast path may return vertices whose PARENT
            // edges are missing/stale in the global GRAPH (e.g. after
            // many Replaces).  Retry with the global DFS which walks the
            // full alive graph and produces consistent vertices.
            log::debug!(
                "globalize_replace: build_deletion_edges failed for inode={:?} ({} vertices), \
                 retrying with global DFS: {}",
                inode,
                content_vertices.len(),
                e,
            );
            let global_vertices = find_content_vertices_global(ctx.txn(), inode_pos)?;
            match build_deletion_edges(ctx, &global_vertices) {
                Ok(edges) => edges,
                Err(e2) => {
                    log::debug!(
                        "globalize_replace: global DFS also failed for inode={:?}: {}",
                        inode,
                        e2,
                    );
                    return Err(e2);
                }
            }
        }
    };
    let del_ms = step.elapsed().as_millis();

    if find_ms + del_ms > 50 {
        log::warn!(
            "globalize_replace: inode={:?} find_content={}ms ({} vertices) build_deletion={}ms ({} edges)",
            inode, find_ms, content_vertices.len(), del_ms, deletion_edges.len(),
        );
    } else {
        log::debug!(
            "globalize_replace: inode={:?} find_content={}ms ({} vertices) build_deletion={}ms ({} edges)",
            inode, find_ms, content_vertices.len(), del_ms, deletion_edges.len(),
        );
    }

    let deletion = EdgeUpdate {
        edges: deletion_edges,
        inode: position_to_option_hash(inode_pos),
    };

    let insertion = create_content_vertex(ctx, inode, inode_pos, vec![inode_pos], vec![], content)?;

    Ok(GraphOp::Replacement {
        change: deletion,
        replacement: insertion,
        local,
        encoding,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Delete: mark every content vertex as deleted
// ───────────────────────────────────────────────────────────────────────────

/// Mark every content vertex for this file as deleted.
fn globalize_delete<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode: Inode,
    inode_pos: Position<NodeId>,
    local: Local,
    encoding: Option<Encoding>,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let step = std::time::Instant::now();
    let deletion = match delete_all_content(ctx, inode, inode_pos) {
        Ok(d) => d,
        Err(e) => {
            // Same fallback as globalize_replace: retry via global DFS.
            log::debug!(
                "globalize_delete: delete_all_content failed for inode={:?}, \
                 retrying with global DFS: {}",
                inode,
                e,
            );
            let global_vertices = find_content_vertices_global(ctx.txn(), inode_pos)?;
            let deletion_edges = build_deletion_edges(ctx, &global_vertices)?;
            EdgeUpdate {
                edges: deletion_edges,
                inode: position_to_option_hash(inode_pos),
            }
        }
    };
    let del_ms = step.elapsed().as_millis();
    if del_ms > 50 {
        log::warn!(
            "globalize_delete: inode={:?} delete_all_content took {}ms",
            inode,
            del_ms
        );
    }

    Ok(GraphOp::Edit {
        change: Atom::EdgeUpdate(deletion),
        local,
        encoding,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Shared helpers
// ───────────────────────────────────────────────────────────────────────────

/// Build an [`EdgeUpdate`] that marks every content vertex in the file as
/// deleted.
///
/// This is the single place where "find all content vertices → create
/// deletion edges" happens. Both [`globalize_replace`] and
/// [`globalize_delete`] delegate here.
fn delete_all_content<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<EdgeUpdate<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let content_vertices = find_content_vertices(ctx.txn(), inode, inode_pos)?;
    let deletion_edges = build_deletion_edges(ctx, &content_vertices)?;

    Ok(EdgeUpdate {
        edges: deletion_edges,
        inode: position_to_option_hash(inode_pos),
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Insert position classification
// ───────────────────────────────────────────────────────────────────────────

/// The two valid positions for an Insert hunk.
#[derive(Debug)]
enum InsertPosition {
    /// Insert before all existing content.
    ///
    /// `first_content` is the start position of the first content vertex —
    /// used as the successor of the new vertex.
    Prepend { first_content: Position<NodeId> },

    /// Insert after all existing content.
    ///
    /// `last_content_end` is the end position of the last content vertex —
    /// used as the predecessor of the new vertex.
    Append { last_content_end: Position<NodeId> },
}

/// Determine whether an Insert hunk is a Prepend or Append.
///
/// Returns an error if `old_start` falls in the middle of the file — that
/// case should have been consolidated into a Replace upstream.
fn classify_insert<T>(
    txn: &T,
    inode: Inode,
    inode_pos: Position<NodeId>,
    old_start: usize,
    old_line_count: Option<usize>,
) -> GlobalizeResult<InsertPosition>
where
    T: GraphTxnT + InodeGraphOps,
{
    let vertices = collect_sorted_content_vertices(txn, inode, inode_pos)?;

    // Empty file → append (predecessor is the inode itself).
    if vertices.is_empty() {
        return Ok(InsertPosition::Append {
            last_content_end: inode_pos,
        });
    }

    // Prepend: insert before the very first line.
    if old_start == 0 {
        let first_start = Position::new(vertices[0].change, vertices[0].start);
        return Ok(InsertPosition::Prepend {
            first_content: first_start,
        });
    }

    // Append: insert after the very last line.
    let total_old_lines = old_line_count.unwrap_or(vertices.len());
    if old_start >= total_old_lines {
        let last = vertices.last().unwrap();
        let last_end = Position::new(last.change, last.end);
        return Ok(InsertPosition::Append {
            last_content_end: last_end,
        });
    }

    // Middle insertion — this should never arrive here after the upstream
    // consolidation in `record_modified_file`.  Return a clear error so the
    // caller knows something is wrong rather than silently triplicating
    // content.
    Err(GlobalizeError::MissingContext {
        path: "(middle insertion reached globalize_hunk — upstream consolidation was bypassed)"
            .to_string(),
        line: old_start as u64,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Graph vertex / edge helpers
// ───────────────────────────────────────────────────────────────────────────

/// Collect content vertices for a file, sorted by start position.
///
/// Returns non-empty, non-root, non-inode vertices from the alive graph.
fn collect_sorted_content_vertices<T>(
    txn: &T,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT + InodeGraphOps,
{
    let mut vertices = find_content_vertices(txn, inode, inode_pos)?;
    vertices.sort_by(|a, b| a.start.cmp(&b.start));
    Ok(vertices)
}

/// Retrieve every alive content vertex for a file.
///
/// This always uses `retrieve_graph` which traverses via the `GraphTxnT`
/// implementation.  When the caller passes a `ViewGraph`, the traversal
/// respects the view's change filter — only vertices from visible changes
/// are returned.  When the caller passes a bare `ReadTxn`, all vertices
/// are returned.
///
/// **Why not INODE_GRAPH?**  The `INODE_GRAPH` secondary index stores ALL
/// edges regardless of view.  Using it directly would bypass the
/// `ViewGraph` filter, returning vertices from changes that aren't visible
/// in the current view — causing content duplication in the `record()`
/// path.  The INODE_GRAPH optimisation is safe only when there is no
/// filter (e.g. `assemble_and_hash` for git import, which passes a bare
/// `&txn`).  For the general case we must go through `retrieve_graph`.
///
/// **Fast path**: When the INODE_GRAPH secondary index is populated for
/// this inode, we use it directly — O(m) where m = edges for THIS file,
/// instead of O(V+E) global DFS.  This is safe for git import which uses
/// a bare `ReadTxn` (no view filter).
///
/// If the INODE_GRAPH path succeeds but a downstream consumer (e.g.
/// `build_deletion_edges` → `find_predecessor_end`) fails because the
/// returned vertices lack PARENT edges in the global GRAPH, the caller
/// will see a `GlobalizeError`.  To handle this transparently we expose
/// a `_with_fallback` wrapper that retries via the global DFS.
fn find_content_vertices<T>(
    txn: &T,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT + InodeGraphOps,
{
    // Try the INODE_GRAPH fast path first.  This is O(m) in edges for
    // this file vs O(V+E) for the global DFS.
    let populated = txn.inode_graph_is_populated(inode).unwrap_or(false);

    if populated {
        let inode_result = find_content_vertices_inode(txn, inode, inode_pos)?;
        if !inode_result.is_empty() {
            return Ok(inode_result);
        }
        // INODE_GRAPH returned empty — fall through to global DFS.
        log::debug!(
            "find_content_vertices: INODE_GRAPH returned empty for inode={:?}, falling back to global DFS",
            inode,
        );
    }

    // Fall back to global DFS when the secondary index isn't populated
    // or returned no vertices.
    find_content_vertices_global(txn, inode_pos)
}

/// Retrieve content vertices via the INODE_GRAPH secondary index.
///
/// This is the fast path: iterates only edges belonging to this specific
/// file, giving O(m) performance where m is the number of edges for the
/// file, regardless of total graph size.
fn find_content_vertices_inode<T>(
    txn: &T,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT + InodeGraphOps,
{
    use crate::types::EdgeFlags;
    use std::collections::HashSet;

    let step = std::time::Instant::now();

    // Walk forward edges from the inode vertex to discover all alive
    // content vertices.  We use a BFS/DFS through the inode-scoped index.
    let mut visited: HashSet<GraphNode<NodeId>> = HashSet::new();
    let mut stack: Vec<GraphNode<NodeId>> = vec![inode_pos.inode_node()];
    let mut content_vertices: Vec<GraphNode<NodeId>> = Vec::new();

    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }

        // Get forward edges (BLOCK, not PARENT, not DELETED) for this vertex
        // within the inode scope.
        let min_flag = EdgeFlags::BLOCK;
        let max_flag = EdgeFlags::all();
        let mut adj = match txn.init_inode_adj(inode, node, min_flag, max_flag) {
            Ok(a) => a,
            Err(_) => continue,
        };

        while let Some(edge_result) = txn.next_inode_adj(&mut adj) {
            let edge = match edge_result {
                Ok(e) => e,
                Err(_) => continue,
            };

            let flags = edge.flag();
            // Skip PARENT edges (reverse), DELETED edges, and PSEUDO edges
            if flags.contains(EdgeFlags::PARENT)
                || flags.contains(EdgeFlags::DELETED)
                || flags.contains(EdgeFlags::PSEUDO)
            {
                continue;
            }

            // Resolve destination vertex
            let dest_pos = edge.dest();
            let dest_node = match txn.find_block(dest_pos) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if !visited.contains(&dest_node) {
                stack.push(dest_node);

                // Collect non-root, non-inode content vertices
                if !dest_node.change.is_root()
                    && !(dest_node.start == dest_node.end && dest_node.start == inode_pos.pos)
                {
                    content_vertices.push(dest_node);
                }
            }
        }
    }

    let elapsed_ms = step.elapsed().as_millis();
    if elapsed_ms > 50 {
        log::warn!(
            "find_content_vertices_inode: inode={:?} took {}ms ({} content vertices, {} visited)",
            inode,
            elapsed_ms,
            content_vertices.len(),
            visited.len(),
        );
    } else {
        log::debug!(
            "find_content_vertices_inode: inode={:?} took {}ms ({} content vertices, {} visited)",
            inode,
            elapsed_ms,
            content_vertices.len(),
            visited.len(),
        );
    }

    Ok(content_vertices)
}

/// Retrieve content vertices via global GRAPH DFS.
fn find_content_vertices_global<T>(
    txn: &T,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT,
{
    let options = RetrieveOptions::default();
    let step = std::time::Instant::now();
    let result = match retrieve_graph(txn, inode_pos, options) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };
    let retrieve_ms = step.elapsed().as_millis();
    if retrieve_ms > 50 {
        log::warn!(
            "find_content_vertices_global: retrieve_graph took {}ms ({} vertices, {} edges traversed, {} positions visited)",
            retrieve_ms,
            result.graph.len_vertices(),
            result.edges_traversed,
            result.positions_visited,
        );
    } else {
        log::debug!(
            "find_content_vertices_global: retrieve_graph took {}ms ({} vertices, {} edges, {} positions)",
            retrieve_ms,
            result.graph.len_vertices(),
            result.edges_traversed,
            result.positions_visited,
        );
    }

    let mut out = Vec::new();
    for vid in 0..result.graph.len_vertices() {
        let Some(alive) = result.graph.try_get_vertex(vid.into()) else {
            continue;
        };
        let node = alive.node;

        // Skip the virtual root.
        if node.change.is_root() {
            continue;
        }
        // Skip the inode marker (empty vertex at the inode position).
        if node.start == node.end && node.start == inode_pos.pos {
            continue;
        }

        out.push(node);
    }
    Ok(out)
}

/// Create [`NewEdge`] deletion entries for each vertex.
///
/// For every content vertex we:
/// 1. Find its predecessor via PARENT edges.
/// 2. Create a `BLOCK | DELETED` edge from that predecessor to the vertex.
/// 3. Track the dependency on the change that introduced the vertex.
fn build_deletion_edges<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    vertices: &[GraphNode<NodeId>],
) -> GlobalizeResult<Vec<NewEdge<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let mut edges = Vec::with_capacity(vertices.len());

    for &v in vertices {
        ctx.add_dependency_by_id(v.change)?;

        let from_pos = find_predecessor_end(ctx.txn(), v)?;

        edges.push(NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: position_to_option_hash_resolved(ctx.txn(), from_pos, None),
            to: vertex_to_option_hash_resolved(ctx.txn(), v, None),
            introduced_by: find_edge_introduced_by(ctx.txn(), from_pos, v),
        });
    }

    Ok(edges)
}

/// Walk PARENT edges of `node` to find the end position of its predecessor.
fn find_predecessor_end<T: GraphTxnT + InodeGraphOps>(
    txn: &T,
    node: GraphNode<NodeId>,
) -> GlobalizeResult<Position<NodeId>> {
    let min_flag = EdgeFlags::BLOCK | EdgeFlags::PARENT;
    let max_flag = EdgeFlags::BLOCK | EdgeFlags::PARENT | EdgeFlags::FOLDER;

    let mut adj = txn
        .iter_adjacent(node, min_flag, max_flag)
        .map_err(|e| GlobalizeError::Pristine(Box::new(e)))?;

    if let Some(edge) = adj.next() {
        let edge = edge.map_err(|e| GlobalizeError::Pristine(Box::new(e)))?;
        return Ok(edge.dest());
    }

    Err(GlobalizeError::NodeNotFound {
        position: node.start_pos(),
    })
}

/// Discover which change introduced the forward edge from `from_pos` to
/// `to_vertex`.
fn find_edge_introduced_by<T: GraphTxnT + InodeGraphOps>(
    txn: &T,
    from_pos: Position<NodeId>,
    to_vertex: GraphNode<NodeId>,
) -> Option<Hash> {
    let from_vertex = match txn.find_block_end(from_pos) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let min_flag = EdgeFlags::BLOCK;
    let max_flag = EdgeFlags::BLOCK | EdgeFlags::FOLDER;

    let adj = match txn.iter_adjacent(from_vertex, min_flag, max_flag) {
        Ok(a) => a,
        Err(_) => return None,
    };

    for edge_result in adj {
        let edge = match edge_result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if edge.dest() == to_vertex.start_pos() {
            return txn.get_external(edge.introduced_by()).ok().flatten();
        }
    }

    None
}

/// Convert a [`GraphNode<NodeId>`] to [`GraphNode<Option<Hash>>`], resolving
/// external change hashes via the transaction.
fn vertex_to_option_hash_resolved<T: GraphTxnT + InodeGraphOps>(
    txn: &T,
    node: GraphNode<NodeId>,
    current_change_id: Option<NodeId>,
) -> GraphNode<Option<Hash>> {
    let change = if node.change.is_root() {
        Some(Hash::NONE)
    } else if current_change_id == Some(node.change) {
        None
    } else {
        match txn.get_external(node.change) {
            Ok(Some(hash)) => Some(hash),
            _ => None,
        }
    };

    GraphNode {
        change,
        start: node.start,
        end: node.end,
    }
}
