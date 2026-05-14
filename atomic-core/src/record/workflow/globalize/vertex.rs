use super::*;

// VERTEX CREATION

/// Create a Insertion for adding a filename to a parent directory.
///
/// When adding a new file, we first need to add its name as a span
/// in the parent directory's graph. This function creates that span.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `parent_inode` - The inode of the parent directory
/// * `filename` - The filename to add (just the name, not full path)
///
/// # Returns
///
/// A `Insertion` structure ready to be included in a graph_op.
///
/// # Example
///
/// ```rust,ignore
/// let name_vertex = create_name_vertex(&mut ctx, parent_inode, "new_file.rs")?;
/// ```
pub fn create_name_vertex<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    parent_inode: Inode,
    filename: &str,
) -> GlobalizeResult<Insertion<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Get the parent's graph position
    let parent_pos = resolve_inode_to_position(ctx, parent_inode)?;

    // Track dependency on the parent's change
    ctx.add_dependency_by_id(parent_pos.change)?;

    // Append the filename to content buffer
    let filename_bytes = filename.as_bytes();
    let (start, end) = ctx.append_content(filename_bytes);

    // The predecessors is the parent directory's position
    // For a directory entry, we use FOLDER flag
    Ok(Insertion {
        predecessors: vec![position_to_option_hash(parent_pos)],
        successors: vec![],
        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
        start,
        end,
        inode: position_to_option_hash(parent_pos),
    })
}

/// Create a Insertion for a file's inode entry.
///
/// Every file has an inode span that serves as the root of its content
/// graph. This function creates that span.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `name_pos` - Position of the filename span (from `create_name_vertex`)
///
/// # Returns
///
/// A `Insertion` structure for the inode.
///
/// # Note
///
/// The inode span is typically empty (start == end) as it just serves
/// as a reference point for the content graph.
///
/// # Example
///
/// ```rust,ignore
/// let inode_vertex = create_inode_vertex(&mut ctx, name_position)?;
/// ```
pub fn create_inode_vertex<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    name_pos: Position<Option<Hash>>,
) -> GlobalizeResult<Insertion<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Inode span has the name as its predecessors
    // It's an empty span (no content bytes)
    let pos = ChangePosition::new(ctx.content_len());

    Ok(Insertion {
        predecessors: vec![name_pos],
        successors: vec![],
        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
        start: pos,
        end: pos, // Empty span
        inode: name_pos,
    })
}

/// Create a Insertion for file content.
///
/// This creates a span containing actual file content, with proper
/// up and down context for positioning in the graph.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `inode` - The file's inode
/// * `inode_pos` - The graph position of the file's inode
/// * `predecessors` - Positions that should come before this content
/// * `successors` - Positions that should come after this content
/// * `content` - The content bytes
///
/// # Returns
///
/// A `Insertion` structure for the content.
///
/// # Example
///
/// ```rust,ignore
/// let content_vertex = create_content_vertex(
///     &mut ctx,
///     inode,
///     inode_pos,
///     vec![up_pos],
///     vec![down_pos],
///     b"Hello, world!",
/// )?;
/// ```
pub fn create_content_vertex<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    _inode: Inode,
    inode_pos: Position<NodeId>,
    predecessors: Vec<Position<NodeId>>,
    successors: Vec<Position<NodeId>>,
    content: &[u8],
) -> GlobalizeResult<Insertion<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Track dependencies on context vertices
    for pos in &predecessors {
        ctx.add_dependency_by_id(pos.change)?;
    }
    for pos in &successors {
        ctx.add_dependency_by_id(pos.change)?;
    }

    // Append content to buffer
    let (start, end) = ctx.append_content(content);

    // Convert contexts to Option<Hash> positions, resolving external change hashes.
    // For predecessors and successors, we need to use the actual hash of the
    // change that introduced those vertices, not None (which means self-reference).
    // We pass None for current_change_id since we're creating a new change and
    // don't have its NodeId yet - any position not matching will be resolved.
    let up_ctx: Vec<Position<Option<Hash>>> = predecessors
        .into_iter()
        .map(|pos| position_to_option_hash_resolved(ctx.txn(), pos, None))
        .collect();
    let down_ctx: Vec<Position<Option<Hash>>> = successors
        .into_iter()
        .map(|pos| position_to_option_hash_resolved(ctx.txn(), pos, None))
        .collect();

    Ok(Insertion {
        predecessors: up_ctx,
        successors: down_ctx,
        flag: EdgeFlags::BLOCK,
        start,
        end,
        inode: position_to_option_hash_resolved(ctx.txn(), inode_pos, None),
    })
}

/// Create a chain of per-line content vertices.
///
/// Splits `content` on newline boundaries and creates one [`Insertion`] per
/// line, chained via predecessor/successor edges.  This gives the graph
/// line-level granularity so that concurrent edits to different lines
/// don't conflict.
///
/// Returns a `Vec` of `Insertion`s.  The first has the given `predecessors`,
/// the last has the given `successors`, and intermediate ones chain to
/// each other via self-referencing positions within the new change.
///
/// If the content is empty or contains only a single line, a single vertex
/// is created (equivalent to [`create_content_vertex`]).
pub fn create_content_vertices_per_line<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    _inode: Inode,
    inode_pos: Position<NodeId>,
    predecessors: Vec<Position<NodeId>>,
    successors: Vec<Position<NodeId>>,
    content: &[u8],
) -> GlobalizeResult<Vec<Insertion<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Track dependencies on context vertices
    for pos in &predecessors {
        ctx.add_dependency_by_id(pos.change)?;
    }
    for pos in &successors {
        ctx.add_dependency_by_id(pos.change)?;
    }

    // Split content into lines (keeping the \n in each line)
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut start = 0;
    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&content[start..=i]);
            start = i + 1;
        }
    }
    // Remainder after last \n (if content doesn't end with \n)
    if start < content.len() {
        lines.push(&content[start..]);
    }

    // If empty or single line, just create one vertex
    if lines.len() <= 1 {
        let insertion =
            create_content_vertex(ctx, _inode, inode_pos, predecessors, successors, content)?;
        return Ok(vec![insertion]);
    }

    let inode_hash = position_to_option_hash_resolved(ctx.txn(), inode_pos, None);

    let mut insertions = Vec::with_capacity(lines.len());

    for (i, line) in lines.iter().enumerate() {
        let (line_start, line_end) = ctx.append_content(line);

        let up_ctx = if i == 0 {
            // First line: predecessors are the given predecessors
            predecessors
                .iter()
                .map(|pos| position_to_option_hash_resolved(ctx.txn(), *pos, None))
                .collect()
        } else {
            // Subsequent lines: predecessor is the previous line in this change.
            // Use None for change (self-reference) and the end position of
            // the previous line's vertex.
            let prev: &Insertion<Option<Hash>> = &insertions[i - 1];
            vec![Position {
                change: None,
                pos: prev.end,
            }]
        };

        let down_ctx = if i == lines.len() - 1 {
            // Last line: successors are the given successors
            successors
                .iter()
                .map(|pos| position_to_option_hash_resolved(ctx.txn(), *pos, None))
                .collect()
        } else {
            // Not last: no successor (the next line will reference us as predecessor)
            vec![]
        };

        insertions.push(Insertion {
            predecessors: up_ctx,
            successors: down_ctx,
            flag: EdgeFlags::BLOCK,
            start: line_start,
            end: line_end,
            inode: inode_hash.clone(),
        });
    }

    Ok(insertions)
}

// EDGE CREATION (DELETIONS)

/// Create an EdgeUpdate for deleting content.
///
/// When content is deleted, we don't actually remove it from the graph.
/// Instead, we mark the edges leading to that content with the DELETED flag.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `inode` - The file's inode
/// * `inode_pos` - The graph position of the file's inode
/// * `deleted_vertices` - The vertices to mark as deleted
///
/// # Returns
///
/// An `EdgeUpdate` structure that marks the specified content as deleted.
///
/// # Example
///
/// ```rust,ignore
/// let deletion_edges = create_deletion_edges(
///     &mut ctx,
///     inode,
///     inode_pos,
///     deleted_vertices,
/// )?;
/// ```
pub fn create_deletion_edges<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    _inode: Inode,
    inode_pos: Position<NodeId>,
    deleted_vertices: Vec<GraphNode<NodeId>>,
) -> GlobalizeResult<EdgeUpdate<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    let mut edges = Vec::new();

    for deleted_node in deleted_vertices {
        // Track dependency on the change that introduced this span
        ctx.add_dependency_by_id(deleted_node.change)?;

        // Create a deletion edge
        // The edge goes from the start of the span to the span itself
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: position_to_option_hash(deleted_node.start_pos()),
            to: vertex_to_option_hash(deleted_node),
            introduced_by: node_id_to_option_hash(deleted_node.change),
        };
        edges.push(edge);
    }

    Ok(EdgeUpdate {
        edges,
        inode: position_to_option_hash_resolved(ctx.txn(), inode_pos, None),
    })
}
