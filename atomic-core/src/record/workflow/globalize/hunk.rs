use super::*;

// MAIN GLOBALIZATION FUNCTIONS

/// Globalize a single built graph_op into a graph graph_op.
///
/// This is the core function that converts a local working copy change
/// (represented as a `BuiltHunk`) into a graph-compatible `GraphOp<Option<Hash>>`.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `built` - The built graph_op from the recording phase
/// * `inode` - The file's inode
/// * `inode_pos` - The graph position of the file's inode
/// * `content` - The content slice for this graph_op
/// * `full_content` - The full file content (needed for NeedsReplace case)
/// * `old_line_count` - Number of lines in the old content (for precise insert detection)
///
/// # Returns
///
/// A graph-compatible graph_op, or an error if globalization fails.
///
/// # Example
///
/// ```rust,ignore
/// let graph_op = globalize_hunk(&mut ctx, &built_hunk, inode, inode_pos, content)?;
/// change.add_hunk(graph_op);
/// ```
pub fn globalize_hunk<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    built: &BuiltHunk,
    inode: Inode,
    inode_pos: Position<NodeId>,
    content: &[u8],
    full_content: &[u8],
    old_line_count: Option<usize>,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    let local = built.local.clone();
    let encoding = built.encoding;

    // For modifications to existing files, we need to find the correct context
    // positions from the content graph. The predecessors should be the END of the
    // span that comes before, and successors should be the START of the
    // span that comes after.
    //
    // We use the line number information (old_start) to determine insertion position:
    // - old_start == 0: Prepend (insert at beginning)
    // - Otherwise: Check if we can insert in middle, or need to do a Replace
    //
    // The challenge is that without byte-to-span mapping (like original Atomic has),
    // we can't reliably insert in the middle of a single span. So for middle insertions,
    // we convert to Replace (delete all old content, insert all new content).

    match built.kind {
        BuiltHunkKind::Insert => {
            // Pure insertion - create a Insertion
            // Determine predecessors and successors based on insertion position
            // old_start tells us which line in the old content the insertion comes AFTER
            let insert_result =
                find_insert_context(ctx.txn(), inode_pos, built.old_start, old_line_count)?;

            match insert_result {
                InsertContext::Prepend { successors } => {
                    // Prepend: connect to inode, with successors to first content
                    let content_node = create_content_vertex(
                        ctx,
                        inode,
                        inode_pos,
                        vec![inode_pos],
                        successors,
                        content,
                    )?;

                    Ok(GraphOp::Edit {
                        change: Atom::Insertion(content_node),
                        local,
                        encoding,
                    })
                }
                InsertContext::Append { predecessors } => {
                    // Append: connect after existing content
                    let content_node = create_content_vertex(
                        ctx,
                        inode,
                        inode_pos,
                        predecessors,
                        vec![],
                        content,
                    )?;

                    Ok(GraphOp::Edit {
                        change: Atom::Insertion(content_node),
                        local,
                        encoding,
                    })
                }
                InsertContext::NeedsReplace => {
                    // Middle insertion into a single span - we can't split it without
                    // byte-to-span mapping. Convert this to a Replace operation:
                    // 1. Delete all existing content
                    // 2. Insert new content connected to the inode
                    //
                    // This is semantically correct: we're replacing the file content.
                    let content_vertices = find_content_vertices(ctx.txn(), inode_pos)?;
                    let deletion_edges =
                        create_deletion_edges_for_vertices(ctx, &content_vertices)?;

                    let deletion = EdgeUpdate {
                        edges: deletion_edges,
                        inode: position_to_option_hash(inode_pos),
                    };

                    // Insert the FULL file content connected to the inode (after deletion)
                    // We use full_content because this is a complete file replacement
                    let insertion = create_content_vertex(
                        ctx,
                        inode,
                        inode_pos,
                        vec![inode_pos], // predecessors: the inode span itself
                        vec![],          // successors: nothing after
                        full_content,
                    )?;

                    Ok(GraphOp::Replacement {
                        change: deletion,
                        replacement: insertion,
                        local,
                        encoding,
                    })
                }
            }
        }

        BuiltHunkKind::Delete => {
            // Pure deletion - create an EdgeUpdate
            // Find all content vertices for this file and mark them as deleted
            let content_vertices = find_content_vertices(ctx.txn(), inode_pos)?;
            let deletion_edges = create_deletion_edges_for_vertices(ctx, &content_vertices)?;

            let edge_update = EdgeUpdate {
                edges: deletion_edges,
                inode: position_to_option_hash(inode_pos),
            };

            Ok(GraphOp::Edit {
                change: Atom::EdgeUpdate(edge_update),
                local,
                encoding,
            })
        }

        BuiltHunkKind::Replace => {
            // Replacement - delete old content, insert new
            // For a replacement, we need to:
            // 1. Find and delete the old content vertices
            // 2. Insert the FULL new file content connected to the inode span
            //
            // IMPORTANT: We must use `full_content` (the entire new file), not `content`
            // (just the replacement portion). This is because we're deleting ALL old
            // content vertices, so we need to replace with ALL new content.
            //
            // Bug fix: Previously this used `content` which only contained the changed
            // lines, causing data loss of unchanged lines.
            let content_vertices = find_content_vertices(ctx.txn(), inode_pos)?;
            let deletion_edges = create_deletion_edges_for_vertices(ctx, &content_vertices)?;

            let deletion = EdgeUpdate {
                edges: deletion_edges,
                inode: position_to_option_hash(inode_pos),
            };

            // For replacement, insert the FULL new file content connected to the inode
            // (not just the graph_op content, since we're deleting ALL old content)
            let insertion = create_content_vertex(
                ctx,
                inode,
                inode_pos,
                vec![inode_pos], // predecessors: the inode span itself
                vec![],          // successors: nothing after
                full_content,    // Use full file content, not just the graph_op portion
            )?;

            Ok(GraphOp::Replacement {
                change: deletion,
                replacement: insertion,
                local,
                encoding,
            })
        }
    }
}

/// Find the end position of the last content span in a file.
///
/// This traverses the file's content graph to find the span that represents
/// the end of the current content. This position is used as predecessors when
/// appending new content.
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `inode_pos` - The graph position of the file's inode
///
/// # Returns
///
/// The end position of the last content span, or the inode position if
/// the file has no content.

/// Result of finding insert context - determines how to handle the insertion.
#[derive(Debug)]
enum InsertContext {
    /// Prepend: insert at the very beginning of the file.
    /// predecessors should be the inode, successors is the start of first content.
    Prepend { successors: Vec<Position<NodeId>> },
    /// Append: insert at the end of the file.
    /// predecessors is the end of last content, successors is empty.
    Append { predecessors: Vec<Position<NodeId>> },
    /// Middle insertion that requires a Replace operation.
    /// This happens when we need to insert within a single span but don't have
    /// byte-to-span mapping to find the exact position.
    NeedsReplace,
}

/// Find the appropriate context for an insertion based on old_start line number.
///
/// This determines where new content should be inserted:
/// - old_start == 0: Prepend (insert before all existing content)
/// - old_start >= total_lines: Append (insert after all existing content)
/// - Otherwise: Middle insertion (needs Replace because we can't split vertices)
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `inode_pos` - The graph position of the file's inode
/// * `old_start` - The line number in old content AFTER which to insert (0 = prepend)
///
/// # Returns
///
/// An `InsertContext` indicating how to handle the insertion.
fn find_insert_context<T>(
    txn: &T,
    inode_pos: Position<NodeId>,
    old_start: usize,
    old_line_count: Option<usize>,
) -> GlobalizeResult<InsertContext>
where
    T: GraphTxnT,
{
    // Retrieve the file's content graph
    let options = RetrieveOptions::default();
    let result = match retrieve_graph(txn, inode_pos, options) {
        Ok(r) => r,
        Err(_) => {
            // Empty file - treat as append (will connect to inode)
            return Ok(InsertContext::Append {
                predecessors: vec![inode_pos],
            });
        }
    };

    // Collect content vertices with their positions
    let mut content_vertices: Vec<(GraphNode<NodeId>, Position<NodeId>, Position<NodeId>)> =
        Vec::new();

    for vertex_id in 0..result.graph.len_vertices() {
        if let Some(alive_vertex) = result.graph.try_get_vertex(vertex_id.into()) {
            let alive_node = alive_vertex.node;

            // Skip DUMMY and empty vertices
            if alive_node.change.is_root() || alive_node.start == alive_node.end {
                continue;
            }

            let start_pos = Position::new(alive_node.change, alive_node.start);
            let end_pos = Position::new(alive_node.change, alive_node.end);
            content_vertices.push((alive_node, start_pos, end_pos));
        }
    }

    // If no content vertices, this is an empty file - append
    if content_vertices.is_empty() {
        return Ok(InsertContext::Append {
            predecessors: vec![inode_pos],
        });
    }

    // Sort by start position to get proper ordering
    content_vertices.sort_by(|a, b| a.1.pos.cmp(&b.1.pos));

    // old_start == 0 means prepend (insert BEFORE line 0, i.e., at the very beginning)
    if old_start == 0 {
        let first_start = content_vertices[0].1;
        return Ok(InsertContext::Prepend {
            successors: vec![first_start],
        });
    }

    // Determine if this is an append or middle insertion using the actual old line count.
    //
    // old_start indicates which line of OLD content the insertion comes AFTER.
    // If old_start >= total_old_lines, it's an append (insert at end).
    // If old_start < total_old_lines and we have a single span, we need Replace
    // because we can't split a span without byte-to-span mapping.

    // Use the actual old line count if available, otherwise fall back to span count
    let total_old_lines = old_line_count.unwrap_or(content_vertices.len());

    // If old_start >= total lines, it's an append
    if old_start >= total_old_lines {
        let last_end = content_vertices.last().unwrap().2;
        return Ok(InsertContext::Append {
            predecessors: vec![last_end],
        });
    }

    // For single span or middle insertion into multiple vertices,
    // we need byte-level mapping to split correctly.
    // Without it, signal that a Replace is needed.
    Ok(InsertContext::NeedsReplace)
}

#[allow(dead_code)]
fn find_content_end_position<T>(
    txn: &T,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Position<NodeId>>
where
    T: GraphTxnT,
{
    // Retrieve the file's content graph starting from the inode position
    let options = RetrieveOptions::default();
    let result = match retrieve_graph(txn, inode_pos, options) {
        Ok(r) => r,
        Err(_) => {
            // If we can't retrieve the graph, fall back to inode position
            // This can happen for empty files or files with no content yet
            return Ok(inode_pos);
        }
    };

    // Find the span with the highest end position
    // This is the "last" content in the file
    //
    // We track content vertices separately from the inode because inode positions
    // may be in a reserved high range (for CRDT compatibility) that would make
    // simple comparisons fail. We want the content span with the highest end
    // position in the normal content range.
    let mut max_content_end: Option<Position<NodeId>> = None;

    for vertex_id in 0..result.graph.len_vertices() {
        if let Some(alive_vertex) = result.graph.try_get_vertex(vertex_id.into()) {
            let alive_node = &alive_vertex.node;

            // Skip the DUMMY span (NodeId(0) / ROOT)
            if alive_node.change.is_root() {
                continue;
            }

            // Skip empty vertices (like inode markers)
            // Content vertices always have start < end
            if alive_node.start == alive_node.end {
                continue;
            }

            // This is a content span - track the one with the highest end position
            let end_pos = Position::new(alive_node.change, alive_node.end);

            match &max_content_end {
                None => {
                    // First content span found
                    max_content_end = Some(end_pos);
                }
                Some(current_max) => {
                    // Compare by position value - we want the highest end position
                    if end_pos.pos > current_max.pos {
                        max_content_end = Some(end_pos);
                    }
                }
            }
        }
    }

    // Return the highest content end position, or fall back to inode position
    // if no content was found (empty file)
    Ok(max_content_end.unwrap_or(inode_pos))
}

/// Find all content vertices for a file.
///
/// This retrieves the file's graph and returns all non-inode content vertices.
/// Used for deletion operations where we need to mark existing content as deleted.
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `inode_pos` - The graph position of the file's inode
///
/// # Returns
///
/// A vector of content vertices (excluding the inode span and DUMMY).
fn find_content_vertices<T>(
    txn: &T,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT,
{
    use crate::output::alive::{retrieve_graph, RetrieveOptions};

    let options = RetrieveOptions::default();
    let result = match retrieve_graph(txn, inode_pos, options) {
        Ok(r) => r,
        Err(_) => {
            // No graph content - return empty
            return Ok(Vec::new());
        }
    };

    let mut vertices = Vec::new();

    for vertex_id in 0..result.graph.len_vertices() {
        if let Some(alive_vertex) = result.graph.try_get_vertex(vertex_id.into()) {
            let alive_node = alive_vertex.node;

            // Skip DUMMY/ROOT span
            if alive_node.change.is_root() {
                continue;
            }

            // Skip the inode span (empty span at inode position)
            if alive_node.start == alive_node.end && alive_node.start == inode_pos.pos {
                continue;
            }

            // This is a content span
            vertices.push(alive_node);
        }
    }

    Ok(vertices)
}

/// Create deletion edges for a list of content vertices.
///
/// For each span, creates a NewEdge that marks the edge TO that span as deleted.
/// The edge goes from the predecessor's end position to the span being deleted.
///
/// # Arguments
///
/// * `ctx` - The globalization context (for tracking dependencies)
/// * `inode_pos` - The inode position (used to find predecessor edges)
/// * `vertices` - The vertices to mark as deleted
///
/// # Returns
///
/// A vector of NewEdge structures for the deletion.
fn create_deletion_edges_for_vertices<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    vertices: &[GraphNode<NodeId>],
) -> GlobalizeResult<Vec<NewEdge<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT,
{
    use crate::change::NewEdge;
    use crate::types::EdgeFlags;

    let mut edges = Vec::new();

    for v in vertices {
        // Track dependency on the change that introduced this span
        ctx.add_dependency_by_id(v.change)?;

        // Find the predecessor of this span by looking for PARENT edges
        // The deletion edge should go from the predecessor's end to this span
        //
        // For a content span that's a child of the inode, the predecessor
        // is the inode span itself. We look up the parent edge to find
        // the source position.
        let from_pos = find_predecessor_end_position(ctx.txn(), *v)?;

        // Create a deletion edge
        // This marks the edge FROM the predecessor TO this span as deleted
        let edge = NewEdge {
            // The previous edge type (what we expect the existing edge to have)
            previous: EdgeFlags::BLOCK,
            // The new edge type (add DELETED flag)
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            // From: the end position of the predecessor span
            from: position_to_option_hash_resolved(ctx.txn(), from_pos, None),
            // To: the span being deleted
            to: vertex_to_option_hash_resolved(ctx.txn(), *v, None),
            // Introduced by: the change that originally created the edge
            // We look this up from the graph
            introduced_by: find_edge_introduced_by(ctx.txn(), from_pos, *v),
        };

        edges.push(edge);
    }

    Ok(edges)
}

/// Find the end position of the predecessor of a span.
///
/// This looks up the PARENT edges of the span to find which span
/// comes before it, then returns the end position of that predecessor.
fn find_predecessor_end_position<T: GraphTxnT>(
    txn: &T,
    node: GraphNode<NodeId>,
) -> GlobalizeResult<Position<NodeId>> {
    use crate::types::EdgeFlags;

    // Look for BLOCK|PARENT edges - these tell us where the edge came from
    let min_flag = EdgeFlags::BLOCK | EdgeFlags::PARENT;
    let max_flag = EdgeFlags::BLOCK | EdgeFlags::PARENT | EdgeFlags::FOLDER;

    let adj = txn
        .iter_adjacent(node, min_flag, max_flag)
        .map_err(GlobalizeError::Pristine)?;

    for edge_result in adj {
        let edge = edge_result.map_err(GlobalizeError::Pristine)?;

        // The edge dest() points to where the forward edge came FROM
        // (remember, this is a reverse/PARENT edge)
        return Ok(edge.dest());
    }

    // If no parent found, this shouldn't happen for content vertices
    // Use NodeNotFound as the closest matching error type
    Err(GlobalizeError::NodeNotFound {
        position: node.start_pos(),
    })
}

/// Find the change that introduced an edge between two positions.
fn find_edge_introduced_by<T: GraphTxnT>(
    txn: &T,
    from_pos: Position<NodeId>,
    to_vertex: GraphNode<NodeId>,
) -> Option<Hash> {
    use crate::types::EdgeFlags;

    // Find the span at the from position
    let from_vertex = match txn.find_block_end(from_pos) {
        Ok(v) => v,
        Err(_) => return None,
    };

    // Look for the edge from from_vertex to to_vertex
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

        // Check if this edge points to our target span
        if edge.dest() == to_vertex.start_pos() {
            // Get the hash for the introduced_by NodeId
            let introduced_by_id = edge.introduced_by();
            return txn.get_external(introduced_by_id).ok().flatten();
        }
    }

    None
}

/// Convert a GraphNode<NodeId> to GraphNode<Option<Hash>>, resolving external change hashes.
fn vertex_to_option_hash_resolved<T: GraphTxnT>(
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
