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
///                      own slice for Insert, the full new file for Replace)
/// * `old_line_count` – number of lines in the old file (for insert-position
///                      classification)
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
    let content_vertices = find_content_vertices(ctx.txn(), inode, inode_pos)?;
    let deletion_edges = build_deletion_edges(ctx, &content_vertices)?;

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
    let deletion = delete_all_content(ctx, inode, inode_pos)?;

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

/// Retrieve every alive content vertex for a file using the INODE_GRAPH
/// secondary index.
///
/// This is O(m) where m = edges for this file, instead of the O(n)
/// `retrieve_graph` approach that scans the entire global GRAPH.
///
/// Falls back to the global `retrieve_graph` if INODE_GRAPH is not
/// populated for this inode.
fn find_content_vertices<T>(
    txn: &T,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT + InodeGraphOps,
{
    // Try the fast path: INODE_GRAPH secondary index
    let populated = txn.inode_graph_is_populated(inode).unwrap_or(false);

    if populated {
        return find_content_vertices_via_inode(txn, inode, inode_pos);
    }

    // Fallback: global GRAPH scan (for repos where INODE_GRAPH wasn't populated)
    find_content_vertices_global(txn, inode_pos)
}

/// Fast path: scan INODE_GRAPH for this file's vertices only.
fn find_content_vertices_via_inode<T>(
    txn: &T,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT + InodeGraphOps,
{
    use crate::types::EdgeFlags;

    let mut out = Vec::new();

    // Iterate all forward (non-PARENT) edges in this file's INODE_GRAPH.
    // Each unique destination vertex that is alive is a content vertex.
    let mut seen = std::collections::HashSet::new();

    // Start from the inode vertex — follow its forward edges
    let mut stack = vec![inode_pos.inode_node()];
    seen.insert(inode_pos.inode_node());

    while let Some(node) = stack.pop() {
        // Get forward (non-deleted, non-parent) edges for this node
        // within the inode scope.
        let min_flag = EdgeFlags::BLOCK;
        let max_flag = EdgeFlags::all()
            .difference(EdgeFlags::PARENT)
            .difference(EdgeFlags::DELETED);

        let mut adj = match txn.init_inode_adj(inode, node, min_flag, max_flag) {
            Ok(a) => a,
            Err(_) => continue,
        };

        while let Some(edge_result) = txn.next_inode_adj(&mut adj) {
            let edge = match edge_result {
                Ok(e) => e,
                Err(_) => continue,
            };

            let dest_pos = edge.dest();
            let resolved = match txn.find_block(dest_pos) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if seen.contains(&resolved) {
                continue;
            }
            seen.insert(resolved);

            // Skip ROOT
            if resolved.change.is_root() {
                continue;
            }
            // Skip inode marker (empty vertex at inode position)
            if resolved.start == resolved.end && resolved.start == inode_pos.pos {
                continue;
            }

            out.push(resolved);
            stack.push(resolved);
        }
    }

    Ok(out)
}

/// Fallback: retrieve content vertices via global GRAPH DFS.
fn find_content_vertices_global<T>(
    txn: &T,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT,
{
    let options = RetrieveOptions::default();
    let result = match retrieve_graph(txn, inode_pos, options) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };

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
