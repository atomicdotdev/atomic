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
//! | `Insert`  | middle, multi-vertex | **Middle**: insert between two vertices |
//! | `Insert`  | middle, single-vertex | **Fallback**: Replace (re-creates per-line) |
//! | `Replace` | *(always)* | **Replace**: delete all old → insert new per-line |
//! | `Delete`  | *(always)* | **Delete**: delete all old content |
//!
//! Middle insertions into multi-vertex files are resolved to the vertex
//! boundary at `old_start`.  For single-vertex files (first recording),
//! the insert falls back to a whole-file Replace which creates per-line
//! vertices, enabling future middle inserts.
//!
//! Both `Replace` and `Delete` need the same "find every content vertex and
//! build deletion edges" work, so that logic lives in one place:
//! [`delete_all_content`].

use super::*;
use crate::change::Local;
use crate::pristine::InodeGraphOps;

/// Returns `true` for machine-generated files (lockfiles, checksums, bundled
/// output) that should be treated as opaque blobs — skipping CRDT tokenization
/// and using single-vertex graph operations instead of per-line/per-token ops.
pub fn should_use_opaque_generated_vertices(path: &str) -> bool {
    let Some(name) = std::path::Path::new(path).file_name() else {
        return false;
    };
    let name = name.to_string_lossy();

    // Lockfiles and checksums
    if name.ends_with(".lock")
        || name.ends_with(".sum")
        || matches!(
            name.as_ref(),
            "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml"
        )
    {
        return true;
    }

    // Minified / bundled output (typically machine-generated, huge, not human-edited)
    if name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".bundle.js")
        || name.ends_with(".chunk.js")
    {
        return true;
    }

    // Bundled output in well-known output directories.
    // Check both "/dir/" (nested) and "dir/" (root-relative) patterns.
    let path_lower = path.to_lowercase();
    let in_output_dir = ["dist/", "public/", "build/", "out/"]
        .iter()
        .any(|dir| path_lower.starts_with(dir) || path_lower.contains(&format!("/{dir}")));
    if in_output_dir && (name.ends_with(".js") || name.ends_with(".css") || name.ends_with(".map"))
    {
        return true;
    }

    false
}

/// Line-count threshold for the diff above which CRDT tokenization is
/// skipped in favor of line-level-only graph ops.  This measures the total
/// number of *changed* lines (inserted + deleted + replaced), not the file
/// size.  A large file with a 5-line edit stays under the threshold; a
/// bundle rebuild that rewrites 2000 lines exceeds it.
pub const CRDT_DIFF_LINE_THRESHOLD: usize = 500;

// ───────────────────────────────────────────────────────────────────────────
// Public entry point
// ───────────────────────────────────────────────────────────────────────────

/// Globalize a single built hunk into one or more graph operations.
///
/// # Arguments
///
/// * `ctx`            – globalization context (content buffer, dependencies, txn)
/// * `built`          – the built hunk from the recording phase
/// * `inode`          – the file's stable identifier
/// * `inode_pos`      – the graph position of the file's inode vertex
/// * `content`        – the hunk-specific content slice (the inserted/replaced
///   bytes for Insert/Replace, empty for Delete)
/// * `old_line_count` – number of lines in the old file (for insert-position
///   classification)
/// * `full_content`   – the complete new file content, used as a fallback when
///   a middle Insert cannot be performed granularly (single-vertex file) and
///   must be escalated to a whole-file Replace
pub fn globalize_hunk<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    built: &BuiltHunk,
    inode: Inode,
    inode_pos: Position<NodeId>,
    content: &[u8],
    old_line_count: Option<usize>,
    full_content: &[u8],
) -> GlobalizeResult<Vec<GraphOp<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let local = built.local.clone();
    let encoding = built.encoding;

    log::debug!(
        "globalize_hunk: kind={:?} old_start={} new_start={} new_len={} content_len={} full_content_len={} old_line_count={:?}",
        built.kind, built.old_start, built.new_start, built.new_len,
        content.len(), full_content.len(), old_line_count,
    );

    match built.kind {
        BuiltHunkKind::Insert => {
            match globalize_insert(
                ctx,
                built,
                inode,
                inode_pos,
                content,
                old_line_count,
                local.clone(),
                encoding,
            ) {
                Ok(ops) => Ok(ops),
                Err(e) => {
                    log::debug!(
                        "globalize_hunk: insert failed ({:?}), falling back to replace",
                        e
                    );
                    // Middle insert into a single-vertex file — fall back to
                    // a whole-file Replace using the full file content (not
                    // just the inserted slice).
                    globalize_replace(ctx, built, inode, inode_pos, full_content, local, encoding)
                }
            }
        }
        BuiltHunkKind::Replace => {
            globalize_replace(ctx, built, inode, inode_pos, content, local, encoding)
        }
        BuiltHunkKind::Delete => globalize_delete(ctx, built, inode, inode_pos, local, encoding),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Insert: prepend / append (the only two safe positions)
// ───────────────────────────────────────────────────────────────────────────

/// Classify an Insert hunk and emit the corresponding graph operation(s).
///
/// Three positions are supported:
///
/// * **Prepend** (`old_start == 0`): new content is wired between the inode
///   vertex and the first existing content vertex.
/// * **Append** (`old_start >= old_line_count`): new content is wired after
///   the last existing content vertex.
/// * **Middle** (`0 < old_start < old_line_count`, multi-vertex file): new
///   content is wired between two adjacent content vertices.
///
/// For single-vertex files, `classify_insert` returns an error and the
/// caller (`globalize_hunk`) falls back to a whole-file Replace.
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
) -> GlobalizeResult<Vec<GraphOp<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let vertices = collect_sorted_content_vertices_cached(ctx, inode, inode_pos)?;
    let position = classify_insert(&vertices, inode_pos, built.old_start, old_line_count)?;

    let (predecessors, successors) = match position {
        InsertPosition::Prepend { first_content } => (vec![inode_pos], vec![first_content]),
        InsertPosition::Append { last_content_end } => (vec![last_content_end], vec![]),
        InsertPosition::Middle {
            predecessor_end,
            successor_start,
        } => (vec![predecessor_end], vec![successor_start]),
    };

    if should_use_opaque_generated_vertices(&local.path) {
        let insertion =
            create_content_vertex(ctx, inode, inode_pos, predecessors, successors, content)?;
        return Ok(vec![GraphOp::Edit {
            change: Atom::Insertion(insertion),
            local,
            encoding,
        }]);
    }

    let insertions =
        create_content_vertices_per_line(ctx, inode, inode_pos, predecessors, successors, content)?;

    let ops = insertions
        .into_iter()
        .map(|vertex| GraphOp::Edit {
            change: Atom::Insertion(vertex),
            local: local.clone(),
            encoding,
        })
        .collect();

    Ok(ops)
}

// ───────────────────────────────────────────────────────────────────────────
// Replace: delete all old content, insert full new content
// ───────────────────────────────────────────────────────────────────────────

/// Replace the specific line vertices identified by `built.deleted_lines`
/// with new per-line vertices for the replacement content.
///
/// This is **proper patch theory**: only the vertices corresponding to the
/// changed lines are deleted, and only the new lines are inserted.  Lines
/// outside the replaced range stay as the original vertices, shared by
/// every view that includes the parent change.
///
/// # Mapping line numbers to vertices
///
/// Content is stored as a chain of per-line vertices (see
/// `create_content_vertices_per_line`).  After sorting by start position,
/// `vertices[i]` corresponds to line `i` of the old file.
///
/// # Behaviour
///
/// 1. Find the predecessor: the end of the vertex BEFORE the first deleted
///    line.  If the deletion starts at line 0, the predecessor is the inode.
/// 2. Find the successor: the start of the vertex AFTER the last deleted
///    line.  If the deletion extends to the end of the file, there is no
///    successor.
/// 3. Delete only the vertices in the deleted range.
/// 4. Insert per-line vertices for the new content, wired between the
///    predecessor and successor.
///
/// # Fallback
///
/// If the file is stored as a single monolithic vertex (legacy state
/// before per-line vertex creation), we fall back to whole-file Replace.
#[allow(clippy::too_many_arguments)]
fn globalize_replace<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    built: &BuiltHunk,
    inode: Inode,
    inode_pos: Position<NodeId>,
    content: &[u8],
    local: Local,
    encoding: Option<Encoding>,
) -> GlobalizeResult<Vec<GraphOp<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    if should_use_opaque_generated_vertices(&local.path) {
        return globalize_replace_whole_file(ctx, inode, inode_pos, content, local, encoding);
    }

    let sorted = collect_sorted_content_vertices_cached(ctx, inode, inode_pos)?;

    // Legacy: file has no visible content vertices in this view.  Fall
    // back to whole-file replace, which walks INODE_GRAPH unfiltered.
    //
    // NOTE: we use `== 0`, not `<= 1`.  A single-line file produces
    // exactly one per-line vertex, and the targeted path below handles
    // that just fine (predecessor=inode, successor=None, delete that
    // one vertex, insert the replacement lines).  Falling back to
    // whole-file replace for single-line files would have us re-read
    // edges through the unfiltered INODE_GRAPH index, which picks up
    // vertices from OTHER views and silently adds their changes as
    // dependencies — producing a phantom supersession relationship
    // between the just-recorded change and the other view's edits.
    if sorted.is_empty() {
        return globalize_replace_whole_file(ctx, inode, inode_pos, content, local, encoding);
    }

    // Pure insertion (no lines deleted) — treat as a middle/append insert.
    // This happens when the diff produces a Replace with old_len == 0
    // (an insertion that the algorithm classified as a replacement).
    if built.deleted_lines.is_empty() {
        let position =
            match classify_insert(&sorted, inode_pos, built.old_start, Some(sorted.len())) {
                Ok(p) => p,
                Err(_) => {
                    return globalize_replace_whole_file(
                        ctx, inode, inode_pos, content, local, encoding,
                    );
                }
            };

        let (predecessors, successors) = match position {
            InsertPosition::Prepend { first_content } => (vec![inode_pos], vec![first_content]),
            InsertPosition::Append { last_content_end } => (vec![last_content_end], vec![]),
            InsertPosition::Middle {
                predecessor_end,
                successor_start,
            } => (vec![predecessor_end], vec![successor_start]),
        };

        let insertions = create_content_vertices_per_line(
            ctx,
            inode,
            inode_pos,
            predecessors,
            successors,
            content,
        )?;

        let ops = insertions
            .into_iter()
            .map(|vertex| GraphOp::Edit {
                change: Atom::Insertion(vertex),
                local: local.clone(),
                encoding,
            })
            .collect();

        return Ok(ops);
    }

    // The deleted_lines field is contiguous (sorted ascending) by
    // construction in the hunk builder.
    let first_deleted = *built.deleted_lines.first().unwrap();
    let last_deleted = *built.deleted_lines.last().unwrap();

    // Bounds-check.  If the deletion extends beyond what we have stored
    // as per-line vertices, fall back to whole-file replace.
    if last_deleted >= sorted.len() {
        if std::env::var("ATOMIC_TRACE_HYPER").is_ok() {
            eprintln!(
                "FALLBACK whole_file_replace: last_deleted={} sorted.len={}",
                last_deleted,
                sorted.len()
            );
        }
        return globalize_replace_whole_file(ctx, inode, inode_pos, content, local, encoding);
    }

    // Resolve predecessor (end of vertex before the first deleted line).
    let predecessor = if first_deleted == 0 {
        inode_pos
    } else {
        let v = &sorted[first_deleted - 1];
        Position::new(v.change, v.end)
    };

    // Resolve successor (start of vertex after the last deleted line).
    let successor = if last_deleted + 1 < sorted.len() {
        let v = &sorted[last_deleted + 1];
        Some(Position::new(v.change, v.start))
    } else {
        None
    };

    // Build deletion edges only for the targeted vertices.
    let to_delete: Vec<GraphNode<NodeId>> =
        (first_deleted..=last_deleted).map(|i| sorted[i]).collect();
    let deletion_edges = build_deletion_edges(ctx, &to_delete)?;

    let deletion = EdgeUpdate {
        edges: deletion_edges,
        inode: position_to_option_hash_resolved(ctx.txn(), inode_pos, None),
    };

    // Build per-line replacement insertions wired between predecessor and
    // successor (or to nothing if successor is None — append).
    let predecessors = vec![predecessor];
    let successors = successor.into_iter().collect();

    let insertions =
        create_content_vertices_per_line(ctx, inode, inode_pos, predecessors, successors, content)?;

    let mut ops = Vec::with_capacity(insertions.len());
    let mut iter = insertions.into_iter();

    if let Some(first) = iter.next() {
        ops.push(GraphOp::Replacement {
            change: deletion,
            replacement: first,
            local: local.clone(),
            encoding,
        });
        for insertion in iter {
            ops.push(GraphOp::Edit {
                change: Atom::Insertion(insertion),
                local: local.clone(),
                encoding,
            });
        }
    } else {
        // Pure deletion (no replacement content) — just emit the EdgeUpdate.
        ops.push(GraphOp::Edit {
            change: Atom::EdgeUpdate(deletion),
            local,
            encoding,
        });
    }

    Ok(ops)
}

/// Legacy fallback: delete all content vertices and re-insert the entire
/// file as a chain of per-line vertices.  Used when the file is stored as
/// a single monolithic vertex (no line-level structure to target).
fn globalize_replace_whole_file<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode: Inode,
    inode_pos: Position<NodeId>,
    content: &[u8],
    local: Local,
    encoding: Option<Encoding>,
) -> GlobalizeResult<Vec<GraphOp<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    // Delegates to `delete_all_content` for "find every content vertex,
    // delete it" — see its docs for why this always uses the thorough
    // SCC-aware traversal rather than a linear walk (POMO-2/2b).
    let deletion = delete_all_content(ctx, inode_pos)?;

    let insertions = if should_use_opaque_generated_vertices(&local.path) {
        vec![create_content_vertex(
            ctx,
            inode,
            inode_pos,
            vec![inode_pos],
            vec![],
            content,
        )?]
    } else {
        create_content_vertices_per_line(ctx, inode, inode_pos, vec![inode_pos], vec![], content)?
    };

    let mut ops = Vec::with_capacity(insertions.len());
    let mut iter = insertions.into_iter();

    if let Some(first) = iter.next() {
        ops.push(GraphOp::Replacement {
            change: deletion,
            replacement: first,
            local: local.clone(),
            encoding,
        });
        for insertion in iter {
            ops.push(GraphOp::Edit {
                change: Atom::Insertion(insertion),
                local: local.clone(),
                encoding,
            });
        }
    }

    Ok(ops)
}

// ───────────────────────────────────────────────────────────────────────────
// Delete: mark every content vertex as deleted
// ───────────────────────────────────────────────────────────────────────────

/// Mark every content vertex for this file as deleted.
/// Delete the specific line vertices identified by `built.deleted_lines`.
///
/// Proper patch theory: only the targeted vertices are marked deleted.
/// Unaffected lines stay alive as the original vertices.
///
/// Falls back to whole-file deletion for legacy single-vertex files or
/// when `deleted_lines` is empty (full-file Delete hunk).
fn globalize_delete<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    built: &BuiltHunk,
    inode: Inode,
    inode_pos: Position<NodeId>,
    local: Local,
    encoding: Option<Encoding>,
) -> GlobalizeResult<Vec<GraphOp<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    if should_use_opaque_generated_vertices(&local.path) {
        let content_vertices = find_content_vertices(ctx.txn(), inode, inode_pos)?;
        let deletion_edges = build_deletion_edges(ctx, &content_vertices)?;
        let deletion = EdgeUpdate {
            edges: deletion_edges,
            inode: position_to_option_hash_resolved(ctx.txn(), inode_pos, None),
        };
        return Ok(vec![GraphOp::Edit {
            change: Atom::EdgeUpdate(deletion),
            local,
            encoding,
        }]);
    }

    let sorted = collect_sorted_content_vertices_cached(ctx, inode, inode_pos)?;

    // Targeted deletion when we have per-line vertices and a specific
    // deleted range.
    if sorted.len() > 1 && !built.deleted_lines.is_empty() {
        let first_deleted = *built.deleted_lines.first().unwrap();
        let last_deleted = *built.deleted_lines.last().unwrap();

        if last_deleted < sorted.len() {
            let to_delete: Vec<GraphNode<NodeId>> =
                (first_deleted..=last_deleted).map(|i| sorted[i]).collect();
            let deletion_edges = build_deletion_edges(ctx, &to_delete)?;

            let deletion = EdgeUpdate {
                edges: deletion_edges,
                inode: position_to_option_hash_resolved(ctx.txn(), inode_pos, None),
            };

            return Ok(vec![GraphOp::Edit {
                change: Atom::EdgeUpdate(deletion),
                local,
                encoding,
            }]);
        }
    }

    // Fallback: whole-file deletion. `delete_all_content` already does the
    // thorough search (POMO-2/2b), so no further fallback is needed here.
    let deletion = delete_all_content(ctx, inode_pos)?;

    Ok(vec![GraphOp::Edit {
        change: Atom::EdgeUpdate(deletion),
        local,
        encoding,
    }])
}

// ───────────────────────────────────────────────────────────────────────────
// Shared helpers
// ───────────────────────────────────────────────────────────────────────────

/// Build an [`EdgeUpdate`] that marks every content vertex in the file as
/// deleted.
///
/// This is the single place where "find all content vertices → create
/// deletion edges" happens. Both [`globalize_replace_whole_file`] and
/// [`globalize_delete`] delegate here.
///
/// Uses [`find_content_vertices_global`] — the full SCC-aware traversal via
/// `retrieve_graph`, the same one used for display/diffing — rather than a
/// linear walk (`collect_sorted_content_vertices`, used elsewhere for
/// targeted range lookups). A linear walk only follows one BLOCK-edge
/// successor chain from the inode, so a genuine fork (two chains both
/// anchored on the inode — exactly what the orphan-view duplication bug,
/// POMO-1, produces) leaves one branch unvisited. It doesn't error on that
/// incomplete result, so callers that only fall back to the global search on
/// an `Err` never actually reach it: `build_deletion_edges` happily succeeds
/// on whatever partial list it's given, deleting only that branch and
/// leaving the other alive (POMO-2). This function's whole purpose is
/// deleting *every* content vertex, so it always pays for the thorough
/// traversal — it's a rare, non-hot-path fallback (opaque/legacy files,
/// whole-file replace, or a full-file delete), so the extra cost is an
/// acceptable trade for actually being complete.
fn delete_all_content<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<EdgeUpdate<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let content_vertices = find_content_vertices_global(ctx.txn(), inode_pos)?;
    let deletion_edges = build_deletion_edges(ctx, &content_vertices)?;

    Ok(EdgeUpdate {
        edges: deletion_edges,
        inode: position_to_option_hash_resolved(ctx.txn(), inode_pos, None),
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Insert position classification
// ───────────────────────────────────────────────────────────────────────────

/// The valid positions for an Insert hunk.
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

    /// Insert between two existing content vertices.
    ///
    /// `predecessor_end` is the end position of the vertex before the
    /// insertion point.  `successor_start` is the start position of the
    /// vertex after the insertion point.
    Middle {
        predecessor_end: Position<NodeId>,
        successor_start: Position<NodeId>,
    },
}

/// Determine whether an Insert hunk is a Prepend, Append, or Middle insert.
///
/// For multi-vertex files (files that have been recorded at least once with
/// per-line vertex granularity), a middle insert is resolved to the vertex
/// boundary at `old_start`.  For single-vertex files there are no internal
/// boundaries, so we return an error and let the caller fall back to Replace.
fn classify_insert(
    vertices: &[GraphNode<NodeId>],
    inode_pos: Position<NodeId>,
    old_start: usize,
    old_line_count: Option<usize>,
) -> GlobalizeResult<InsertPosition> {
    log::debug!(
        "classify_insert: old_start={} old_line_count={:?} vertices={}",
        old_start,
        old_line_count,
        vertices.len(),
    );

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

    // ── Middle insertion ──
    //
    // For single-vertex files we cannot split at a line boundary, so we
    // return an error.  The caller (`globalize_insert`) catches this and
    // falls back to a whole-file Replace.
    if vertices.len() == 1 {
        return Err(GlobalizeError::MissingContext {
            path: "(middle insertion into single-vertex file — needs consolidation)".to_string(),
            line: old_start as u64,
        });
    }

    // Multi-vertex file: walk vertices to find the boundary.
    // After per-line vertex creation, each vertex corresponds roughly
    // to one line, so vertex[old_start-1] / vertex[old_start] gives
    // the correct boundary.
    if old_start < vertices.len() {
        let pred = &vertices[old_start - 1];
        let succ = &vertices[old_start];
        return Ok(InsertPosition::Middle {
            predecessor_end: Position::new(pred.change, pred.end),
            successor_start: Position::new(succ.change, succ.start),
        });
    }

    // old_start >= vertices.len() but < total_old_lines.
    // This can happen when line counting and vertex counting diverge.
    // Fall back to append after the last vertex.
    let last = vertices.last().unwrap();
    let last_end = Position::new(last.change, last.end);
    Ok(InsertPosition::Append {
        last_content_end: last_end,
    })
}

fn collect_sorted_content_vertices_cached<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT + InodeGraphOps,
{
    if let Some(vertices) = ctx.content_vertices_cache.get(&inode) {
        return Ok(vertices.clone());
    }

    let vertices = collect_sorted_content_vertices(ctx.txn, inode, inode_pos)?;
    ctx.content_vertices_cache.insert(inode, vertices.clone());
    Ok(vertices)
}

// ───────────────────────────────────────────────────────────────────────────
// Graph vertex / edge helpers
// ───────────────────────────────────────────────────────────────────────────

/// Collect content vertices for a file in **graph traversal order**.
///
/// Starts at the inode vertex and walks forward BLOCK edges, producing
/// vertices in the order they appear in the file.  This is the only
/// correct ordering when vertices come from multiple changes — sorting
/// by `start` would interleave vertices from different changes whose
/// position spaces are independent.
///
/// Skips DELETED edges and PSEUDO edges.  Stops at empty vertices that
/// aren't the inode.
fn collect_sorted_content_vertices<T>(
    txn: &T,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT + InodeGraphOps,
{
    use crate::types::EdgeFlags;
    use std::collections::HashSet;

    if !txn.inode_graph_needs_view_filter() && txn.inode_graph_is_populated(inode).unwrap_or(false)
    {
        let step = std::time::Instant::now();
        let ordered = collect_sorted_content_vertices_inode_ordered(txn, inode, inode_pos);
        if !ordered.is_empty() {
            let elapsed_ms = step.elapsed().as_millis();
            if elapsed_ms > 50 {
                log::warn!(
                    "collect_sorted_content_vertices: inode fast path took {}ms (inode={:?}, vertices={})",
                    elapsed_ms,
                    inode,
                    ordered.len(),
                );
            }
            return Ok(ordered);
        }
    }
    let mut ordered: Vec<GraphNode<NodeId>> = Vec::new();
    let mut visited: HashSet<GraphNode<NodeId>> = HashSet::new();
    let mut current = inode_pos.inode_node();

    // Helper: is `node` dead in this view?
    //
    // We check parent edges through the same (view-filtered) `txn`.
    // Any visible `BLOCK|DELETED` parent means a change inside the
    // view deleted this vertex — it should be skipped during the
    // line-order walk because it's no longer a live line.
    let is_dead_in_view = |node: GraphNode<NodeId>| -> bool {
        let parents = match txn.iter_adjacent(node, EdgeFlags::PARENT, EdgeFlags::all()) {
            Ok(it) => it,
            Err(_) => return false,
        };
        for e in parents {
            let e = match e {
                Ok(e) => e,
                Err(_) => continue,
            };
            let flags = e.flag();
            if flags.contains(EdgeFlags::PARENT) && flags.contains(EdgeFlags::DELETED) {
                return true;
            }
        }
        false
    };

    fn alive_reaches<T: GraphTxnT + InodeGraphOps>(
        txn: &T,
        inode: Inode,
        start: GraphNode<NodeId>,
        target: GraphNode<NodeId>,
        is_dead_in_view: &dyn Fn(GraphNode<NodeId>) -> bool,
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

            let edges = match txn.iter_forward(current, false) {
                Ok(edges) => edges,
                Err(_) => continue,
            };

            for edge in edges {
                if edge.kind.is_pseudo() {
                    continue;
                }
                let dest = txn
                    .find_block_in_inode(inode, edge.dest)
                    .ok()
                    .flatten()
                    .or_else(|| txn.find_block(edge.dest).ok());
                let Some(dest) = dest else {
                    continue;
                };
                if is_dead_in_view(dest) {
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

    loop {
        if !visited.insert(current) {
            break;
        }

        // Find the next alive BLOCK child of `current`.
        let min_flag = EdgeFlags::BLOCK;
        let max_flag = EdgeFlags::BLOCK | EdgeFlags::FOLDER;

        let adj = match txn.iter_adjacent(current, min_flag, max_flag) {
            Ok(a) => a,
            Err(_) => break,
        };

        // Prefer alive destinations. When there are multiple alive children,
        // choose the one that reaches another alive child downstream. This
        // preserves linear order for cases like:
        //   console.log(...) -> metrics -> }    and
        //   console.log(...) -> }
        // where the first encountered child may be the downstream `}` rather
        // than the upstream `metrics` line we need to index next.
        let mut alive_candidates: Vec<GraphNode<NodeId>> = Vec::new();
        let mut next_dead: Option<GraphNode<NodeId>> = None;
        for edge_result in adj {
            let edge = match edge_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let flags = edge.flag();
            if flags.contains(EdgeFlags::PARENT)
                || flags.contains(EdgeFlags::DELETED)
                || flags.contains(EdgeFlags::PSEUDO)
            {
                continue;
            }
            let dest = txn
                .find_block_in_inode(inode, edge.dest())
                .ok()
                .flatten()
                .or_else(|| txn.find_block(edge.dest()).ok());
            let Some(dest) = dest else {
                continue;
            };
            if visited.contains(&dest) {
                continue;
            }
            if !is_dead_in_view(dest) {
                alive_candidates.push(dest);
            } else if next_dead.is_none() {
                next_dead = Some(dest);
            }
        }

        let next_alive = if alive_candidates.len() <= 1 {
            alive_candidates.into_iter().next()
        } else {
            alive_candidates
                .iter()
                .copied()
                .find(|candidate| {
                    let reaches_other = alive_candidates.iter().copied().any(|other| {
                        other != *candidate
                            && alive_reaches(txn, inode, *candidate, other, &is_dead_in_view)
                    });
                    let reached_by_other = alive_candidates.iter().copied().any(|other| {
                        other != *candidate
                            && alive_reaches(txn, inode, other, *candidate, &is_dead_in_view)
                    });
                    reaches_other && !reached_by_other
                })
                .or_else(|| {
                    alive_candidates.iter().copied().find(|candidate| {
                        alive_candidates.iter().copied().any(|other| {
                            other != *candidate
                                && alive_reaches(txn, inode, *candidate, other, &is_dead_in_view)
                        })
                    })
                })
                .or_else(|| alive_candidates.into_iter().next())
        };

        let dest = match next_alive.or(next_dead) {
            Some(d) => d,
            None => break,
        };

        let is_inode_marker = dest.start == dest.end && dest.start == inode_pos.pos;
        let is_alive = !is_dead_in_view(dest);
        if is_alive && !is_inode_marker && !dest.change.is_root() && dest.start != dest.end {
            ordered.push(dest);
        }

        current = dest;
    }

    log::debug!(
        "collect_sorted_content_vertices: inode={:?} produced {} ordered vertices",
        inode,
        ordered.len(),
    );

    if std::env::var("ATOMIC_TRACE_HYPER").is_ok() {
        eprintln!("SORTED {} entries:", ordered.len());
        let mut total = 0u64;
        for (i, v) in ordered.iter().enumerate() {
            let size = v.end.get() - v.start.get();
            total += size;
            eprintln!("  [{:3}] {:?} size={}", i, v, size);
        }
        eprintln!("  total content bytes: {}", total);
    }

    let _ = inode;
    Ok(ordered)
}

fn collect_sorted_content_vertices_inode_ordered<T>(
    txn: &T,
    inode: Inode,
    inode_pos: Position<NodeId>,
) -> Vec<GraphNode<NodeId>>
where
    T: GraphTxnT + InodeGraphOps,
{
    use crate::types::EdgeFlags;
    use std::collections::HashSet;

    let mut ordered: Vec<GraphNode<NodeId>> = Vec::new();
    let mut visited: HashSet<GraphNode<NodeId>> = HashSet::new();
    let mut current = inode_pos.inode_node();

    let is_dead_in_inode = |node: GraphNode<NodeId>| -> bool {
        let mut parents = match txn.init_inode_adj(
            node_inode_or(inode),
            node,
            EdgeFlags::PARENT,
            EdgeFlags::all(),
        ) {
            Ok(adj) => adj,
            Err(_) => return false,
        };
        while let Some(edge) = txn.next_inode_adj(&mut parents) {
            let Ok(edge) = edge else {
                continue;
            };
            let flags = edge.flag();
            if flags.contains(EdgeFlags::PARENT) && flags.contains(EdgeFlags::DELETED) {
                return true;
            }
        }
        false
    };

    fn alive_reaches_inode<T: GraphTxnT + InodeGraphOps>(
        txn: &T,
        inode: Inode,
        start: GraphNode<NodeId>,
        target: GraphNode<NodeId>,
        is_dead: &dyn Fn(GraphNode<NodeId>) -> bool,
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

            let mut adj = match txn.init_inode_adj(
                current_inode_or(inode),
                current,
                EdgeFlags::BLOCK,
                EdgeFlags::all(),
            ) {
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
                {
                    continue;
                }

                let dest = txn
                    .find_block_in_inode(inode, edge.dest())
                    .ok()
                    .flatten()
                    .or_else(|| txn.find_block(edge.dest()).ok());
                let Some(dest) = dest else {
                    continue;
                };
                if is_dead(dest) {
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

    loop {
        if !visited.insert(current) {
            break;
        }

        let mut adj = match txn.init_inode_adj(
            inode,
            current,
            EdgeFlags::BLOCK,
            EdgeFlags::BLOCK | EdgeFlags::FOLDER,
        ) {
            Ok(adj) => adj,
            Err(_) => break,
        };

        let mut alive_candidates: Vec<GraphNode<NodeId>> = Vec::new();
        let mut next_dead: Option<GraphNode<NodeId>> = None;

        while let Some(edge) = txn.next_inode_adj(&mut adj) {
            let Ok(edge) = edge else {
                continue;
            };
            let flags = edge.flag();
            if flags.contains(EdgeFlags::PARENT)
                || flags.contains(EdgeFlags::DELETED)
                || flags.contains(EdgeFlags::PSEUDO)
            {
                continue;
            }

            let dest = txn
                .find_block_in_inode(inode, edge.dest())
                .ok()
                .flatten()
                .or_else(|| txn.find_block(edge.dest()).ok());
            let Some(dest) = dest else {
                continue;
            };
            if visited.contains(&dest) {
                continue;
            }
            if !is_dead_in_inode(dest) {
                alive_candidates.push(dest);
            } else if next_dead.is_none() {
                next_dead = Some(dest);
            }
        }

        let next_alive = if alive_candidates.len() <= 1 {
            alive_candidates.into_iter().next()
        } else {
            alive_candidates
                .iter()
                .copied()
                .find(|candidate| {
                    let reaches_other = alive_candidates.iter().copied().any(|other| {
                        other != *candidate
                            && alive_reaches_inode(txn, inode, *candidate, other, &is_dead_in_inode)
                    });
                    let reached_by_other = alive_candidates.iter().copied().any(|other| {
                        other != *candidate
                            && alive_reaches_inode(txn, inode, other, *candidate, &is_dead_in_inode)
                    });
                    reaches_other && !reached_by_other
                })
                .or_else(|| {
                    alive_candidates.iter().copied().find(|candidate| {
                        alive_candidates.iter().copied().any(|other| {
                            other != *candidate
                                && alive_reaches_inode(
                                    txn,
                                    inode,
                                    *candidate,
                                    other,
                                    &is_dead_in_inode,
                                )
                        })
                    })
                })
                .or_else(|| alive_candidates.into_iter().next())
        };

        let dest = match next_alive.or(next_dead) {
            Some(dest) => dest,
            None => break,
        };

        let is_inode_marker = dest.start == dest.end && dest.start == inode_pos.pos;
        let is_alive = !is_dead_in_inode(dest);
        if is_alive && !is_inode_marker && !dest.change.is_root() && dest.start != dest.end {
            ordered.push(dest);
        }

        current = dest;
    }

    ordered
}

fn node_inode_or(inode: Inode) -> Inode {
    inode
}

fn current_inode_or(inode: Inode) -> Inode {
    inode
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
            let dest_node = txn
                .find_block_in_inode(inode, dest_pos)
                .ok()
                .flatten()
                .or_else(|| txn.find_block(dest_pos).ok());
            let Some(dest_node) = dest_node else {
                continue;
            };

            if !visited.contains(&dest_node) {
                stack.push(dest_node);

                // Collect non-root, non-inode content vertices
                if !(dest_node.change.is_root()
                    || (dest_node.start == dest_node.end && dest_node.start == inode_pos.pos))
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
