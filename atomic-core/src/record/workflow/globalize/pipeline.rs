use super::*;


/// Globalize all hunks in a recorded file.
///
/// This processes all hunks in a `RecordedFile` and converts them to
/// graph-compatible hunks.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `recorded` - The recorded file with built hunks
/// * `options` - Globalization options
///
/// # Returns
///
/// A `GlobalizedFile` containing all the converted hunks.
///
/// # Example
///
/// ```rust,ignore
/// let globalized = globalize_recorded_file(&mut ctx, &recorded_file, &options)?;
/// for graph_op in globalized.hunks() {
///     change.add_hunk(graph_op.clone());
/// }
/// ```
pub fn globalize_recorded_file<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    recorded: &RecordedFile,
    options: &GlobalizeOptions,
) -> GlobalizeResult<GlobalizedFile>
where
    T: GraphTxnT + TreeTxnT,
{
    use crate::change::{GraphOp, Insertion};
    use crate::types::{ChangePosition, EdgeFlags, Position};

    let path = recorded.path();
    let mut result = GlobalizedFile::new(path);

    // Handle directory additions (DirAdd)
    if recorded.is_directory() {
        // Create a DirAdd graph_op for an explicitly tracked directory
        // The directory has no content, just name and inode vertices

        let parent_context_pos: Position<Option<Hash>> = {
            let parent_path = extract_parent(path);
            if parent_path.is_empty() {
                // Top-level directory - parent is ROOT
                Position {
                    change: Some(Hash::NONE),
                    pos: ChangePosition::ROOT,
                }
            } else {
                // Nested directory - for now use ROOT
                Position {
                    change: Some(Hash::NONE),
                    pos: ChangePosition::ROOT,
                }
            }
        };

        // Add the directory name to the content buffer
        let dirname = extract_filename(path);
        let dirname_bytes = dirname.as_bytes();
        let (name_start, name_end) = ctx.append_content(dirname_bytes);

        // The inode span is empty (marks the directory's root)
        let inode_start = name_end;
        let inode_end = inode_start;

        // Create name span with FOLDER flag
        let add_name = Insertion {
            predecessors: vec![parent_context_pos],
            successors: vec![],
            flag: EdgeFlags::FOLDER, // FOLDER flag for directory entry
            start: name_start,
            end: name_end,
            inode: Position {
                change: None, // Self-reference (current change)
                pos: inode_start,
            },
        };

        // Create inode span (empty)
        let add_inode = Insertion {
            predecessors: vec![Position {
                change: None,
                pos: name_end,
            }],
            successors: vec![],
            flag: EdgeFlags::FOLDER,
            start: inode_start,
            end: inode_end,
            inode: Position {
                change: None,
                pos: inode_start,
            },
        };

        let graph_op: GraphOp<Option<Hash>> = GraphOp::DirAdd {
            add_name,
            add_inode,
            path: path.to_string(),
        };

        result.add_hunk(graph_op);
        result.set_bytes_added(dirname_bytes.len() as u64);
        return Ok(result);
    }

    // Handle directory deletions (DirDel)
    if recorded.is_deleted_directory() {
        // For directory deletion, we need to create an EdgeUpdate to mark the
        // directory's edges as deleted. This requires the directory's inode
        // and position in the graph.
        //
        // If we have position info, create a proper DirDel graph_op.
        // Otherwise, the directory is already untracked from the TREE table
        // during the record process, so we can skip the graph_op.

        if let (Some(_inode), Some(position)) = (recorded.inode(), recorded.position()) {
            // Convert NodeId to Option<Hash> for the graph_op
            // We need to look up the external hash for this change
            let change_hash: Option<Hash> = ctx.get_external(position.change);

            // Create EdgeUpdate to mark directory edges as deleted
            let del = EdgeUpdate {
                edges: vec![NewEdge {
                    previous: EdgeFlags::FOLDER,
                    flag: EdgeFlags::FOLDER | EdgeFlags::DELETED,
                    from: Position {
                        change: change_hash,
                        pos: position.pos,
                    },
                    to: GraphNode {
                        change: change_hash,
                        start: position.pos,
                        end: position.pos, // Empty span for directory inode
                    },
                    introduced_by: change_hash,
                }],
                inode: Position {
                    change: change_hash,
                    pos: position.pos,
                },
            };

            let graph_op: GraphOp<Option<Hash>> = GraphOp::DirDel {
                del,
                path: path.to_string(),
            };

            result.add_hunk(graph_op);
            // Note: edges_added tracking not implemented in GlobalizedFile
            // The graph_op count serves as a proxy for tracking edge modifications
        }
        // If no position info, the directory was never recorded to the graph,
        // so there's nothing to delete. The tracking removal is sufficient.

        return Ok(result);
    }

    // Check for empty file
    if recorded.is_empty() && !options.include_empty_files() {
        return Ok(result);
    }

    let content = recorded.content();
    let initial_deps = ctx.dependencies().len();
    let initial_content_len = ctx.content_len();

    // Check if this is a newly added file (FileAdd) or a modification
    if let Some(inode) = recorded.inode() {
        // Existing file - needs position for modification
        let inode_pos = recorded
            .position()
            .ok_or_else(|| GlobalizeError::MissingField {
                path: path.to_string(),
                field: "position",
            })?;

        // Track content positions for each hunk to enrich FileOps later
        let mut hunk_content_ranges: Vec<HunkContentRange> = Vec::new();

        // Process each graph_op for modification
        for built in recorded.hunks() {
            // Get the content slice for this graph_op
            let hunk_content =
                if let (Some(start), Some(end)) = (built.content_start, built.content_end) {
                    let start = start as usize;
                    let end = end as usize;
                    if end <= content.len() {
                        &content[start..end]
                    } else {
                        &[]
                    }
                } else {
                    &[]
                };

            // Track content position before globalization
            let content_pos_before = ctx.content_len();

            let graph_op = globalize_hunk(
                ctx,
                built,
                inode,
                inode_pos,
                hunk_content,
                content,
                recorded.old_line_count(),
            )?;

            // Track content position after globalization
            let content_pos_after = ctx.content_len();

            // Record the content range for this hunk
            if content_pos_after > content_pos_before {
                hunk_content_ranges.push(HunkContentRange {
                    kind: built.kind,
                    new_start: built.new_start,
                    new_len: built.new_len,
                    content_start: ChangePosition::new(content_pos_before as u64),
                    content_end: ChangePosition::new(content_pos_after as u64),
                    // For Replace hunks, we use full_content, so track that
                    uses_full_content: matches!(
                        built.kind,
                        crate::record::workflow::graph_op::BuiltHunkKind::Replace
                    ) || matches!(
                        built.kind,
                        crate::record::workflow::graph_op::BuiltHunkKind::Insert
                    ),
                });
            }

            result.add_hunk(graph_op);
        }

        // Enrich FileOps with content ranges for Edit hunks
        if let Some(mut file_ops) = recorded.crdt_ops().cloned() {
            enrich_file_ops_for_edit(&mut file_ops, content, &hunk_content_ranges);
            result.set_file_ops(file_ops);
        }
    } else {
        // Newly added file (FileAdd) - no existing inode/position
        // We need to create a FileAdd graph_op that:
        // 1. Creates the file entry in the parent directory (or root)
        // 2. Contains the file content
        //
        // The FileAdd graph_op structure:
        // - add_name: Span for the filename, connected to parent directory
        // - add_inode: Span for the file's inode (root of file content graph)
        // - contents: Span containing the actual file content

        // Determine the parent context position.
        // For top-level files (no directory prefix), we use ROOT.
        // For nested files, we would resolve the parent directory's position.
        //
        // The ROOT position is represented as:
        // Position { change: Some(Hash::NONE), pos: ChangePosition::ROOT }
        //
        // This is the virtual root span that all top-level files reference.
        let parent_context_pos: Position<Option<Hash>> = {
            let parent_path = extract_parent(path);
            if parent_path.is_empty() {
                // Top-level file - parent is ROOT
                Position {
                    change: Some(Hash::NONE), // Hash::NONE indicates ROOT
                    pos: ChangePosition::ROOT,
                }
            } else {
                // Nested file - try to resolve parent directory position
                // For now, use ROOT as we don't have nested directory support yet
                // In a full implementation, we would resolve the parent directory's
                // inode and get its graph position
                Position {
                    change: Some(Hash::NONE),
                    pos: ChangePosition::ROOT,
                }
            }
        };

        // Add the filename to the content buffer
        let filename = extract_filename(path);
        let filename_bytes = filename.as_bytes();
        let (name_start, name_end) = ctx.append_content(filename_bytes);

        // The inode span is empty (marks the file's root in the graph)
        let inode_start = name_end;
        let inode_end = name_end;

        // Position referencing the END of the name span we're creating (self-reference).
        // Up-context positions must reference the END of the predecessor vertex so that
        // find_block_end() correctly resolves to this name vertex V[name_start:name_end].
        // Using name_start here would cause find_block_end(name_start) to find whatever
        // vertex ENDS at that position (e.g., the previous file's content vertex),
        // creating cross-file edges that contaminate graph traversal.
        // None means "this change" - the actual hash is filled in during serialization.
        let name_pos: Position<Option<Hash>> = Position {
            change: None, // Self-reference to this change
            pos: name_end,
        };

        // Position referencing the inode span we're creating (self-reference)
        let inode_pos: Position<Option<Hash>> = Position {
            change: None, // Self-reference to this change
            pos: inode_start,
        };

        if !content.is_empty() {
            // Add file content to the context buffer
            let (content_start, content_end) = ctx.append_content(content);

            let encoding = recorded.encoding();

            // Enrich FileOps with the content range if available
            // This links the CRDT branches to their graph vertex positions
            if let Some(mut file_ops) = recorded.crdt_ops().cloned() {
                // For a FileAdd, all line content is in the single content span
                // We need to compute per-line ranges within the content
                enrich_file_ops_for_add(&mut file_ops, content, content_start);
                result.set_file_ops(file_ops);
            }

            let graph_op = GraphOp::FileAdd {
                add_name: Insertion {
                    // Parent context - ROOT for top-level files
                    predecessors: vec![parent_context_pos.clone()],
                    successors: vec![],
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    start: name_start,
                    end: name_end,
                    // The inode field for add_name points to the parent's position
                    inode: parent_context_pos,
                },
                add_inode: Insertion {
                    // The inode span's parent is the name span
                    predecessors: vec![name_pos],
                    successors: vec![],
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    start: inode_start,
                    end: inode_end,
                    // The inode field points to itself (this is the file's root)
                    inode: inode_pos.clone(),
                },
                contents: Some(Insertion {
                    // Content's parent is the inode span
                    predecessors: vec![inode_pos.clone()],
                    successors: vec![],
                    flag: EdgeFlags::BLOCK,
                    start: content_start,
                    end: content_end,
                    // Content belongs to this file (referenced by inode)
                    inode: inode_pos,
                }),
                path: path.to_string(),
                encoding,
            };

            result.add_hunk(graph_op);
        } else if options.include_empty_files() {
            // Empty file - still create the FileAdd but with no content span
            let graph_op = GraphOp::FileAdd {
                add_name: Insertion {
                    predecessors: vec![parent_context_pos.clone()],
                    successors: vec![],
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    start: name_start,
                    end: name_end,
                    inode: parent_context_pos,
                },
                add_inode: Insertion {
                    predecessors: vec![name_pos],
                    successors: vec![],
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    start: inode_start,
                    end: inode_end,
                    inode: inode_pos.clone(),
                },
                contents: None,
                path: path.to_string(),
                encoding: recorded.encoding(),
            };

            result.add_hunk(graph_op);
        }
    }

    // Note: FileOps enrichment for modifications is now handled above
    // in the inode branch after processing all hunks

    // Update statistics
    result.set_bytes_added(ctx.content_len() - initial_content_len);
    result.set_dependency_count(ctx.dependencies().len() - initial_deps);

    Ok(result)
}

/// Enrich FileOps with content ranges for a FileAdd operation.
///
/// For new files, the content is laid out sequentially in the change buffer.
/// This function computes the byte range for each line within the content
/// and stores it in the LineOps for later use in populating BRANCH_VERTEX.
fn enrich_file_ops_for_add(
    file_ops: &mut crate::change::FileOps,
    content: &[u8],
    content_start: ChangePosition,
) {
    use crate::types::ChangePosition;

    // Split content into lines to compute per-line ranges
    let mut line_start = 0usize;
    let mut line_idx = 0usize;

    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' {
            // End of line (including the newline)
            let line_end = i + 1;

            // Find the corresponding LineOps entry by line number
            if let Some(line_ops) = file_ops
                .line_ops_mut()
                .iter_mut()
                .find(|ops| ops.new_line_num() == Some(line_idx + 1))
            {
                // Compute the absolute positions in the change content buffer
                let abs_start = ChangePosition::new(content_start.get() + line_start as u64);
                let abs_end = ChangePosition::new(content_start.get() + line_end as u64);
                line_ops.set_content_range(abs_start, abs_end);
            }

            line_start = line_end;
            line_idx += 1;
        }
    }

    // Handle last line if it doesn't end with newline
    if line_start < content.len() {
        if let Some(line_ops) = file_ops
            .line_ops_mut()
            .iter_mut()
            .find(|ops| ops.new_line_num() == Some(line_idx + 1))
        {
            let abs_start = ChangePosition::new(content_start.get() + line_start as u64);
            let abs_end = ChangePosition::new(content_start.get() + content.len() as u64);
            line_ops.set_content_range(abs_start, abs_end);
        }
    }
}

/// Tracks content range information for a globalized hunk.
///
/// Used to correlate hunks with LineOps during Edit enrichment.
#[allow(dead_code)]
#[derive(Debug)]
struct HunkContentRange {
    /// The kind of hunk (Insert, Delete, Replace).
    kind: crate::record::workflow::graph_op::BuiltHunkKind,
    /// Starting line number in new content (0-indexed).
    new_start: usize,
    /// Number of lines in new content.
    new_len: usize,
    /// Start position in the change content buffer.
    content_start: ChangePosition,
    /// End position in the change content buffer.
    content_end: ChangePosition,
    /// Whether this hunk uses the full file content (Replace/Insert with NeedsReplace).
    uses_full_content: bool,
}

/// Enrich FileOps with content ranges for Edit (modification) operations.
///
/// For file modifications, hunks may be Insert, Delete, or Replace operations.
/// This function correlates the hunks with LineOps based on line numbers and
/// computes the byte ranges for inserted content.
///
/// # Arguments
///
/// * `file_ops` - The FileOps to enrich
/// * `content` - The full new file content
/// * `hunk_ranges` - Content range information from globalized hunks
fn enrich_file_ops_for_edit(
    file_ops: &mut crate::change::FileOps,
    content: &[u8],
    hunk_ranges: &[HunkContentRange],
) {
    // For modifications, we have two cases:
    // 1. Simple Insert hunks: Content is the inserted lines only
    // 2. Replace hunks (including NeedsReplace): Content is the full file
    //
    // We need to compute per-line byte ranges within the content that was
    // actually written to the change buffer.

    // Check if any hunk uses full content (Replace/NeedsReplace)
    let uses_full_content = hunk_ranges
        .iter()
        .any(|h| h.uses_full_content && h.new_len > 0);

    if uses_full_content {
        // For Replace hunks, the full file content was written
        // Find the hunk that contains the full content
        if let Some(range) = hunk_ranges
            .iter()
            .find(|h| h.uses_full_content && h.new_len > 0)
        {
            // The content buffer contains the full new file
            // Compute per-line ranges similar to FileAdd
            enrich_lines_from_full_content(file_ops, content, range.content_start);
        }
    } else {
        // For simple Insert hunks, each hunk contains only its inserted lines
        // We need to correlate each hunk's line range with LineOps
        for range in hunk_ranges {
            if range.new_len == 0 {
                continue; // Delete-only hunk, no content
            }

            // This hunk inserts lines [new_start, new_start + new_len)
            // The content for these lines is at [content_start, content_end)
            enrich_lines_from_hunk_content(
                file_ops,
                content,
                range.new_start,
                range.new_len,
                range.content_start,
                range.content_end,
            );
        }
    }
}

/// Enrich LineOps when the full file content was written (Replace scenario).
fn enrich_lines_from_full_content(
    file_ops: &mut crate::change::FileOps,
    content: &[u8],
    content_start: ChangePosition,
) {
    // This is the same logic as enrich_file_ops_for_add
    let mut line_start = 0usize;
    let mut line_idx = 0usize;

    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' {
            let line_end = i + 1;

            // Find the corresponding LineOps entry by line number (1-indexed)
            if let Some(line_ops) = file_ops
                .line_ops_mut()
                .iter_mut()
                .find(|ops| ops.new_line_num() == Some(line_idx + 1))
            {
                let abs_start = ChangePosition::new(content_start.get() + line_start as u64);
                let abs_end = ChangePosition::new(content_start.get() + line_end as u64);
                line_ops.set_content_range(abs_start, abs_end);
            }

            line_start = line_end;
            line_idx += 1;
        }
    }

    // Handle last line if it doesn't end with newline
    if line_start < content.len() {
        if let Some(line_ops) = file_ops
            .line_ops_mut()
            .iter_mut()
            .find(|ops| ops.new_line_num() == Some(line_idx + 1))
        {
            let abs_start = ChangePosition::new(content_start.get() + line_start as u64);
            let abs_end = ChangePosition::new(content_start.get() + content.len() as u64);
            line_ops.set_content_range(abs_start, abs_end);
        }
    }
}

/// Enrich LineOps for a specific hunk's inserted content.
///
/// This handles the case where a hunk only contains its inserted lines,
/// not the full file content.
fn enrich_lines_from_hunk_content(
    file_ops: &mut crate::change::FileOps,
    full_content: &[u8],
    hunk_new_start: usize,
    hunk_new_len: usize,
    content_start: ChangePosition,
    content_end: ChangePosition,
) {
    // Extract the slice of content that corresponds to this hunk
    // We need to find the byte range in full_content for lines [hunk_new_start, hunk_new_start + hunk_new_len)

    // First, find the byte offset in full_content for hunk_new_start
    let mut byte_offset = 0usize;
    let mut current_line = 0usize;

    for (i, &byte) in full_content.iter().enumerate() {
        if current_line == hunk_new_start {
            byte_offset = i;
            break;
        }
        if byte == b'\n' {
            current_line += 1;
        }
    }

    // Now process lines within the hunk's range
    let mut line_start_in_hunk = 0usize; // Relative to the hunk's content in the buffer
    let mut lines_processed = 0usize;

    // Iterate through full_content starting from the hunk's starting line
    let hunk_content_len = (content_end.get() - content_start.get()) as usize;
    let hunk_slice_start = byte_offset;
    let hunk_slice_end = (byte_offset + hunk_content_len).min(full_content.len());

    if hunk_slice_start >= full_content.len() {
        return;
    }

    let hunk_slice = &full_content[hunk_slice_start..hunk_slice_end];

    for (i, &byte) in hunk_slice.iter().enumerate() {
        if byte == b'\n' {
            let line_end_in_hunk = i + 1;
            let actual_line_num = hunk_new_start + lines_processed;

            // Find the corresponding LineOps entry (1-indexed)
            if let Some(line_ops) = file_ops
                .line_ops_mut()
                .iter_mut()
                .find(|ops| ops.new_line_num() == Some(actual_line_num + 1))
            {
                let abs_start =
                    ChangePosition::new(content_start.get() + line_start_in_hunk as u64);
                let abs_end = ChangePosition::new(content_start.get() + line_end_in_hunk as u64);
                line_ops.set_content_range(abs_start, abs_end);
            }

            line_start_in_hunk = line_end_in_hunk;
            lines_processed += 1;

            if lines_processed >= hunk_new_len {
                break;
            }
        }
    }

    // Handle last line if it doesn't end with newline
    if lines_processed < hunk_new_len && line_start_in_hunk < hunk_slice.len() {
        let actual_line_num = hunk_new_start + lines_processed;

        if let Some(line_ops) = file_ops
            .line_ops_mut()
            .iter_mut()
            .find(|ops| ops.new_line_num() == Some(actual_line_num + 1))
        {
            let abs_start = ChangePosition::new(content_start.get() + line_start_in_hunk as u64);
            let abs_end = content_end;
            line_ops.set_content_range(abs_start, abs_end);
        }
    }
}
