use super::*;
use crate::pristine::{InodeGraphOps, PathClaimId};

impl<'txn, T> GlobalizeContext<'txn, T>
where
    T: GraphTxnT + TreeTxnT,
{
    /// Build a FileMove from the exact currently alive structural source claim.
    ///
    /// The claimant and source edge are validated before globalization. This is
    /// required after a rename because the stable inode position still points to
    /// the original add and cannot reconstruct the current name vertex.
    pub fn file_move_from_claim(
        &mut self,
        path: &str,
        claimant: Position<NodeId>,
        source: PathClaimId,
        destination_parent: Position<Option<Hash>>,
    ) -> GlobalizeResult<GraphOp<Option<Hash>>> {
        if source.claimant != claimant || source.name.is_empty() || !source.parent.is_empty() {
            return Err(GlobalizeError::InvalidParentMetadata {
                path: path.to_string(),
                parent: extract_parent(path).to_string(),
                reason: "FileMove source claim does not identify one exact directory name edge",
            });
        }

        let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
        let mut source_edge_exists = false;
        for edge in self
            .txn()
            .iter_adjacent(source.parent, EdgeFlags::empty(), EdgeFlags::all())?
        {
            let edge = edge?;
            source_edge_exists |= edge.flag() == alive
                && edge.dest() == source.name.start_pos()
                && edge.introduced_by() == source.introduced_by;
        }
        let mut claimant_edge_exists = false;
        for edge in self
            .txn()
            .iter_adjacent(source.name, EdgeFlags::empty(), EdgeFlags::all())?
        {
            let edge = edge?;
            claimant_edge_exists |= edge.flag() == alive && edge.dest() == claimant;
        }
        if !source_edge_exists || !claimant_edge_exists {
            return Err(GlobalizeError::InvalidParentMetadata {
                path: path.to_string(),
                parent: extract_parent(path).to_string(),
                reason: "FileMove source claim is not the exact currently alive claimant",
            });
        }

        let source_from = self.external_existing_position(source.parent.end_pos())?;
        let source_name = GraphNode {
            change: self.external_existing_change(source.name.change)?,
            start: source.name.start,
            end: source.name.end,
        };
        let introduced_by = self.external_existing_change(source.introduced_by)?;
        let claimant = self.external_existing_position(claimant)?;
        let del = EdgeUpdate {
            edges: vec![NewEdge {
                previous: alive,
                flag: alive | EdgeFlags::DELETED,
                from: source_from,
                to: source_name,
                introduced_by,
            }],
            inode: claimant,
        };

        let filename = extract_filename(path).as_bytes();
        let (start, end) = self.append_content(filename);
        let add = Insertion {
            predecessors: vec![destination_parent],
            successors: vec![claimant],
            flag: alive,
            start,
            end,
            inode: claimant,
        };
        Ok(GraphOp::FileMove {
            del,
            add,
            path: path.to_string(),
        })
    }

    fn external_existing_position(
        &mut self,
        position: Position<NodeId>,
    ) -> GlobalizeResult<Position<Option<Hash>>> {
        Ok(Position {
            change: self.external_existing_change(position.change)?,
            pos: position.pos,
        })
    }

    fn external_existing_change(&mut self, change: NodeId) -> GlobalizeResult<Option<Hash>> {
        if change.is_root() {
            return Ok(Some(Hash::NONE));
        }
        self.add_dependency_by_id(change)?;
        self.get_external(change)
            .map(Some)
            .ok_or(GlobalizeError::MissingExternalHash { node_id: change })
    }
}

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
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    use crate::change::{GraphOp, Insertion};
    use crate::types::{ChangePosition, EdgeFlags, Position};

    let path = recorded.path();
    let mut result = GlobalizedFile::new(path);

    // Undelete must run before the ordinary Added-directory branch. A restored
    // empty directory deliberately has the same surface shape as a new one, but
    // it must reverse the visible deletion and retain the original inode.
    if recorded.is_undelete() {
        let initial_deps = ctx.dependencies().len();
        let undel = build_undelete_edge_update(ctx, recorded)?;
        if recorded.is_undeleted_directory() {
            if undel.edges.len() != 2 {
                return Err(GlobalizeError::InvalidParentMetadata {
                    path: path.to_string(),
                    parent: extract_parent(path).to_string(),
                    reason: "directory undelete did not resolve both structural claims",
                });
            }
            result.add_hunk(GraphOp::DirUndel {
                undel,
                path: path.to_string(),
            });
            result.set_dependency_count(ctx.dependencies().len() - initial_deps);
            return Ok(result);
        }

        result.add_hunk(GraphOp::FileUndel {
            undel,
            contents: None,
            path: path.to_string(),
            encoding: recorded.encoding(),
        });
        if recorded.hunks().is_empty() {
            result.set_dependency_count(ctx.dependencies().len() - initial_deps);
            return Ok(result);
        }
    }

    // Handle directory additions (DirAdd). Missing ancestors are emitted as
    // implicit directory anchors before the requested directory, so input order
    // cannot flatten nested names under ROOT.
    if recorded.is_directory() {
        let initial_content_len = ctx.content_len();
        let initial_deps = ctx.dependencies().len();
        ensure_directory_anchor(ctx, path, path, &mut result)?;
        result.set_bytes_added(ctx.content_len() - initial_content_len);
        result.set_dependency_count(ctx.dependencies().len() - initial_deps);
        return Ok(result);
    }

    // A directory deletion removes the two precise claims emitted by DirAdd:
    // parent -> name and name -> inode. Positions alone are ambiguous here
    // because the name and empty inode marker share an end position.
    if recorded.is_deleted_directory() {
        let initial_deps = ctx.dependencies().len();
        let del = build_directory_delete(ctx, recorded)?;
        result.add_hunk(GraphOp::DirDel {
            del,
            path: path.to_string(),
        });
        result.set_dependency_count(ctx.dependencies().len() - initial_deps);
        return Ok(result);
    }

    // Exact moves emit their FileMove before entering the ordinary existing-file
    // content path below. Track their contribution separately because the
    // content-path baselines are captured after this structural operation.
    let mut leading_bytes_added = 0;
    let mut leading_dependency_count = 0;

    // Handle file moves/renames (FileMove)
    if matches!(
        recorded.kind(),
        Some(crate::record::workflow::detect::DetectionKind::Moved)
    ) {
        use crate::change::EdgeUpdate;

        let old_path = recorded
            .old_path()
            .ok_or_else(|| GlobalizeError::MissingField {
                path: path.to_string(),
                field: "old_path",
            })?;

        if let Some(source) = recorded.exact_source_claim() {
            let claimant = recorded
                .position()
                .ok_or_else(|| GlobalizeError::MissingField {
                    path: path.to_string(),
                    field: "position",
                })?;
            let initial_content_len = ctx.content_len();
            let initial_deps = ctx.dependencies().len();
            let destination_parent = ensure_parent_directory_anchor(ctx, path, path, &mut result)?;
            let operation = ctx.file_move_from_claim(path, claimant, source, destination_parent)?;
            result.add_hunk(operation);
            leading_bytes_added = ctx.content_len() - initial_content_len;
            leading_dependency_count = ctx.dependencies().len() - initial_deps;
        } else {
            // Compatibility path for callers that have not supplied the exact
            // causally maximal source claim. This legacy reconstruction cannot
            // safely globalize content edits against an authoritative current
            // name claim, so retain its structural-only early return.
            // Look up the inode for the old path
            let old_inode =
                ctx.txn()
                    .get_inode(old_path)?
                    .ok_or_else(|| GlobalizeError::PathNotFound {
                        path: old_path.to_string(),
                    })?;

            // Look up the inode's graph position
            let old_inode_pos_node = ctx
                .txn()
                .inode_position(old_inode)?
                .ok_or_else(|| GlobalizeError::InodeNotFound { inode: old_inode })?;

            // Convert the inode position to Option<Hash> using external hash resolution
            let change_hash: Option<Hash> = ctx.get_external(old_inode_pos_node.change);

            // Add a dependency on the change that introduced this file
            ctx.add_dependency_by_id(old_inode_pos_node.change)?;

            let old_parent_context_pos: Position<Option<Hash>> = {
                let old_parent_path = extract_parent(old_path);
                if old_parent_path.is_empty() {
                    Position {
                        change: Some(Hash::NONE),
                        pos: ChangePosition::ROOT,
                    }
                } else {
                    match resolve_parent_inode(ctx, old_path)
                        .and_then(|parent_inode| resolve_inode_to_position(ctx, parent_inode))
                    {
                        Ok(parent_pos) => {
                            ctx.add_dependency_by_id(parent_pos.change)?;
                            position_to_option_hash_resolved(ctx.txn(), parent_pos, None)
                        }
                        Err(_) => Position {
                            change: Some(Hash::NONE),
                            pos: ChangePosition::ROOT,
                        },
                    }
                }
            };

            let old_filename = extract_filename(old_path);
            let old_name_end = old_inode_pos_node.pos;
            let old_name_start =
                ChangePosition::new(old_name_end.get().saturating_sub(old_filename.len() as u64));

            // Build the del EdgeUpdate: delete the old parent -> old-name edge.
            //
            // For file adds, the path name is inserted as a normal BLOCK|FOLDER
            // vertex with predecessor = parent context. Renames must delete that
            // edge, not an edge at the inode marker position.
            let del = EdgeUpdate {
                edges: vec![NewEdge {
                    previous: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: old_parent_context_pos,
                    to: GraphNode {
                        change: change_hash,
                        start: old_name_start,
                        end: old_name_end,
                    },
                    introduced_by: change_hash,
                }],
                inode: Position {
                    change: change_hash,
                    pos: old_inode_pos_node.pos,
                },
            };

            // Build the add Insertion: a new name vertex in the correct parent
            // directory, wired to the existing inode position.
            let parent_context_pos: Position<Option<Hash>> = {
                let parent_path = extract_parent(path);
                if parent_path.is_empty() {
                    Position {
                        change: Some(Hash::NONE),
                        pos: ChangePosition::ROOT,
                    }
                } else {
                    match resolve_parent_inode(ctx, path)
                        .and_then(|parent_inode| resolve_inode_to_position(ctx, parent_inode))
                    {
                        Ok(parent_pos) => {
                            ctx.add_dependency_by_id(parent_pos.change)?;
                            position_to_option_hash_resolved(ctx.txn(), parent_pos, None)
                        }
                        Err(_) => Position {
                            change: Some(Hash::NONE),
                            pos: ChangePosition::ROOT,
                        },
                    }
                }
            };

            // The inode position for the add — references the EXISTING inode (old change).
            let inode_opt_hash_pos: Position<Option<Hash>> = Position {
                change: change_hash,
                pos: old_inode_pos_node.pos,
            };

            // Write the new filename bytes into the content buffer.
            let new_filename = extract_filename(path);
            let new_filename_bytes = new_filename.as_bytes();
            let initial_content_len = ctx.content_len();
            let (name_start, name_end) = ctx.append_content(new_filename_bytes);

            let add = Insertion {
                predecessors: vec![parent_context_pos],
                successors: vec![inode_opt_hash_pos],
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                start: name_start,
                end: name_end,
                inode: inode_opt_hash_pos,
            };

            let graph_op: GraphOp<Option<Hash>> = GraphOp::FileMove {
                del,
                add,
                path: path.to_string(),
            };

            result.add_hunk(graph_op);
            result.set_bytes_added(ctx.content_len() - initial_content_len);

            // Carry any CRDT ops that were set on the recorded file
            if let Some(file_ops) = recorded.crdt_ops().cloned() {
                result.set_file_ops(file_ops);
            }

            return Ok(result);
        }
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

        if let Some(retained) = recorded.name_conflict_resolution() {
            ctx.add_dependency_by_id(retained.change)?;
            ctx.add_dependency_by_id(inode_pos.change)?;
            // Selection is explicit; the other operations unlink only the
            // conflicting names. A concurrent rename must keep its content.
            result.add_hunk(GraphOp::SolveNameConflict {
                name: EdgeUpdate {
                    edges: Vec::new(),
                    inode: position_to_option_hash_resolved(ctx.txn(), retained, None),
                },
                path: path.to_string(),
            });
            let mut edges = Vec::new();
            for &name in recorded.name_conflict_bindings() {
                ctx.add_dependency_by_id(name.change)?;
                for parent in ctx.txn().get_edges(name)? {
                    let flag = parent.flag();
                    if !flag.contains(EdgeFlags::PARENT | EdgeFlags::FOLDER)
                        || flag.intersects(EdgeFlags::DELETED | EdgeFlags::PSEUDO)
                    {
                        continue;
                    }
                    ctx.add_dependency_by_id(parent.introduced_by())?;
                    ctx.add_dependency_by_id(parent.dest().change)?;
                    let previous = flag - EdgeFlags::PARENT;
                    edges.push(crate::change::NewEdge {
                        previous,
                        flag: previous | EdgeFlags::DELETED,
                        from: position_to_option_hash_resolved(ctx.txn(), parent.dest(), None),
                        to: GraphNode {
                            change: ctx.get_external(name.change),
                            start: name.start,
                            end: name.end,
                        },
                        introduced_by: ctx.get_external(parent.introduced_by()),
                    });
                }
            }
            if edges.is_empty() {
                return Err(GlobalizeError::MissingField {
                    path: path.to_string(),
                    field: "live name binding for name-conflict resolution",
                });
            }
            result.add_hunk(GraphOp::SolveNameConflict {
                name: EdgeUpdate {
                    edges,
                    inode: position_to_option_hash_resolved(ctx.txn(), inode_pos, None),
                },
                path: path.to_string(),
            });
            if let Some(ops) = recorded.crdt_ops().cloned() {
                result.set_file_ops(ops);
            }
            return Ok(result);
        }

        // Track content positions for each hunk to enrich FileOps later
        let mut hunk_content_ranges: Vec<HunkContentRange> = Vec::new();

        // Process each graph_op for modification
        for built in recorded.hunks() {
            // Determine the content slice for this hunk.
            //
            // Replace hunks need ONLY the replacement lines — passing the
            // full file would cause `globalize_replace` to create per-line
            // vertices for unchanged lines, producing a whole-file rewrite
            // that wipes out the targeted line surgery.  Slice out the lines
            // [new_start, new_start + new_len) from the new content.
            //
            // Insert hunks only need their own slice — they are guaranteed
            // (by the upstream consolidation in record_modified_file) to be
            // clean Prepend or Append operations that wire in their own
            // bytes without touching anything else.
            //
            // Delete hunks carry no content.
            let replace_slice;
            let hunk_content: &[u8] = match built.kind {
                crate::record::workflow::graph_op::BuiltHunkKind::Replace => {
                    replace_slice = slice_lines(content, built.new_start, built.new_len);
                    replace_slice
                }
                crate::record::workflow::graph_op::BuiltHunkKind::Insert => {
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
                    }
                }
                crate::record::workflow::graph_op::BuiltHunkKind::Delete => &[],
            };

            // Track content position before globalization
            let content_pos_before = ctx.content_len();

            let graph_ops = globalize_hunk(
                ctx,
                built,
                inode,
                inode_pos,
                hunk_content,
                recorded.old_line_count(),
                content,
            )?;

            // Track content position after globalization
            let content_pos_after = ctx.content_len();

            // Record the content range for this hunk.
            //
            // `uses_full_content` originally claimed Replace hunks held the
            // full file in their content blob — but globalize slices Replace
            // content to just the replaced lines (`slice_lines(content,
            // built.new_start, built.new_len)` above), so the blob is the
            // hunk-only content.  Setting `uses_full = false` routes us to
            // `enrich_lines_from_hunk_content`, which computes per-line
            // ranges relative to that smaller blob — matching what apply
            // can actually read.
            //
            // (If a future Replace path materializes the full file into the
            // content blob, this is the place to flip the flag back.)
            if content_pos_after > content_pos_before {
                let uses_full = false;
                hunk_content_ranges.push(HunkContentRange {
                    kind: built.kind,
                    new_start: built.new_start,
                    new_len: built.new_len,
                    content_start: ChangePosition::new(content_pos_before),
                    content_end: ChangePosition::new(content_pos_after),
                    uses_full_content: uses_full,
                });
            }

            for graph_op in graph_ops {
                result.add_hunk(graph_op);
            }
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

        let parent_context_pos = ensure_parent_directory_anchor(ctx, path, path, &mut result)?;

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
            let encoding = recorded.encoding();
            let first_content_start = ChangePosition::new(ctx.content_len());

            // Enrich FileOps with the first content position for CRDT.
            if let Some(mut file_ops) = recorded.crdt_ops().cloned() {
                enrich_file_ops_for_add(&mut file_ops, content, first_content_start);
                result.set_file_ops(file_ops);
            }

            let (first_line, line_ranges) = {
                // Split content into per-line slices (each line keeps its '\n').
                // This gives the graph line-level vertex granularity from the
                // very first record, so subsequent edits to different lines
                // can target individual vertices rather than the whole file.
                let line_slices: Vec<&[u8]> = split_into_lines(content);

                // Append each line to the content buffer, recording its range.
                let mut line_ranges: Vec<(ChangePosition, ChangePosition)> =
                    Vec::with_capacity(line_slices.len());
                for line in &line_slices {
                    let (s, e) = ctx.append_content(line);
                    line_ranges.push((s, e));
                }

                let (first_start, first_end) = line_ranges[0];
                (
                    Insertion {
                        predecessors: vec![inode_pos],
                        successors: vec![],
                        flag: EdgeFlags::BLOCK,
                        start: first_start,
                        end: first_end,
                        inode: inode_pos,
                    },
                    line_ranges,
                )
            };

            let graph_op = GraphOp::FileAdd {
                add_name: Insertion {
                    predecessors: vec![parent_context_pos],
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
                    inode: inode_pos,
                },
                contents: Some(first_line),
                path: path.to_string(),
                encoding,
            };
            result.add_hunk(graph_op);

            // Remaining lines: emit as standalone Edit ops chained from
            // the previous line's end position (self-referencing within
            // this change).
            for (i, &(line_start, line_end)) in line_ranges.iter().enumerate().skip(1) {
                let prev_end = line_ranges[i - 1].1;
                let chain_pred = Position {
                    change: None, // self-reference within this change
                    pos: prev_end,
                };
                let line_vertex = Insertion {
                    predecessors: vec![chain_pred],
                    successors: vec![],
                    flag: EdgeFlags::BLOCK,
                    start: line_start,
                    end: line_end,
                    inode: inode_pos,
                };
                result.add_hunk(GraphOp::Edit {
                    change: Atom::Insertion(line_vertex),
                    local: Local::new(path, (i + 1) as u64),
                    encoding,
                });
            }
        } else if options.include_empty_files() {
            // Empty file - still create the FileAdd but with no content span
            let graph_op = GraphOp::FileAdd {
                add_name: Insertion {
                    predecessors: vec![parent_context_pos],
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
                    inode: inode_pos,
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
    result.set_bytes_added(leading_bytes_added + (ctx.content_len() - initial_content_len));
    result
        .set_dependency_count(leading_dependency_count + (ctx.dependencies().len() - initial_deps));

    Ok(result)
}

fn build_directory_delete<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    recorded: &RecordedFile,
) -> GlobalizeResult<EdgeUpdate<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let path = recorded.path();
    let position = recorded
        .position()
        .ok_or_else(|| GlobalizeError::MissingField {
            path: path.to_string(),
            field: "position",
        })?;
    recorded
        .inode()
        .ok_or_else(|| GlobalizeError::MissingField {
            path: path.to_string(),
            field: "inode",
        })?;

    ctx.add_dependency_by_id(position.change)?;
    let change_hash =
        ctx.txn()
            .get_external(position.change)?
            .ok_or(GlobalizeError::MissingExternalHash {
                node_id: position.change,
            })?;
    let dirname = extract_filename(path);
    let name_len = dirname.len() as u64;
    if name_len == 0 || name_len > position.pos.get() {
        return Err(GlobalizeError::InvalidParentMetadata {
            path: path.to_string(),
            parent: extract_parent(path).to_string(),
            reason: "directory name does not match its inode position",
        });
    }
    let name_start = ChangePosition::new(position.pos.get() - name_len);
    let name_node = GraphNode::new(position.change, name_start, position.pos);
    let inode_node = GraphNode::new(position.change, position.pos, position.pos);

    let parent_position = if extract_parent(path).is_empty() {
        Position::ROOT
    } else {
        let parent_inode = resolve_parent_inode(ctx, path)?;
        let parent_position = resolve_inode_to_position(ctx, parent_inode)?;
        ctx.add_dependency_by_id(parent_position.change)?;
        parent_position
    };
    let parent_node = if parent_position.change.is_root() {
        GraphNode::root()
    } else {
        ctx.txn().find_block_end(parent_position)?
    };

    let claim_flags = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    ensure_exact_forward_claim(
        ctx.txn(),
        parent_node,
        name_node,
        claim_flags,
        position.change,
    )?;
    ensure_exact_forward_claim(
        ctx.txn(),
        name_node,
        inode_node,
        claim_flags,
        position.change,
    )?;

    let external_name = GraphNode {
        change: Some(change_hash),
        start: name_start,
        end: position.pos,
    };
    let external_inode = GraphNode {
        change: Some(change_hash),
        start: position.pos,
        end: position.pos,
    };
    let deleted_flags = claim_flags | EdgeFlags::DELETED;
    Ok(EdgeUpdate {
        edges: vec![
            NewEdge {
                previous: claim_flags,
                flag: deleted_flags,
                from: position_to_option_hash_resolved(ctx.txn(), parent_position, None),
                to: external_name,
                introduced_by: Some(change_hash),
            },
            NewEdge {
                previous: claim_flags,
                flag: deleted_flags,
                // Use the name's start so apply can distinguish it from the
                // empty inode marker at name.end.
                from: Position {
                    change: Some(change_hash),
                    pos: name_start,
                },
                to: external_inode,
                introduced_by: Some(change_hash),
            },
        ],
        inode: Position {
            change: Some(change_hash),
            pos: position.pos,
        },
    })
}

fn ensure_exact_forward_claim<T: GraphTxnT>(
    txn: &T,
    source: GraphNode<NodeId>,
    target: GraphNode<NodeId>,
    flags: EdgeFlags,
    introduced_by: NodeId,
) -> GlobalizeResult<()> {
    let mut adjacent = txn.iter_adjacent(source, EdgeFlags::empty(), EdgeFlags::all())?;
    for edge in &mut adjacent {
        let edge = edge?;
        if edge.flag() == flags
            && edge.dest() == target.start_pos()
            && edge.introduced_by() == introduced_by
        {
            return Ok(());
        }
    }
    Err(GlobalizeError::NodeNotFound {
        position: target.start_pos(),
    })
}

fn build_undelete_edge_update<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    recorded: &RecordedFile,
) -> GlobalizeResult<EdgeUpdate<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let inode = recorded
        .inode()
        .ok_or_else(|| GlobalizeError::MissingField {
            path: recorded.path().to_string(),
            field: "inode",
        })?;
    let inode_position = recorded
        .position()
        .ok_or_else(|| GlobalizeError::MissingField {
            path: recorded.path().to_string(),
            field: "position",
        })?;
    let deleting: HashSet<Hash> = recorded.undelete_changes().iter().copied().collect();
    let dirname = extract_filename(recorded.path());
    let directory_name_start = ChangePosition::new(
        inode_position
            .pos
            .get()
            .checked_sub(dirname.len() as u64)
            .unwrap_or(inode_position.pos.get()),
    );
    let inode_node = GraphNode::new(
        inode_position.change,
        inode_position.pos,
        inode_position.pos,
    );

    let mut edges = Vec::new();
    for entry in ctx.txn().iter_inode_vertices(inode)? {
        let (target, reverse) = entry?;
        let flags = reverse.flag();
        if !flags.contains(EdgeFlags::PARENT) || !flags.contains(EdgeFlags::DELETED) {
            continue;
        }
        let deleting_hash = ctx.txn().get_external(reverse.introduced_by())?.ok_or(
            GlobalizeError::MissingExternalHash {
                node_id: reverse.introduced_by(),
            },
        )?;
        if !deleting.contains(&deleting_hash) {
            continue;
        }

        ctx.add_dependency(deleting_hash);
        let mut previous = flags;
        previous.remove(EdgeFlags::PARENT);
        let mut restored = previous;
        restored.remove(EdgeFlags::DELETED);
        let mut source = reverse.dest();
        if recorded.is_undeleted_directory()
            && target == inode_node
            && restored.contains(EdgeFlags::FOLDER)
        {
            source = Position::new(inode_position.change, directory_name_start);
        }
        let edge = NewEdge {
            previous,
            flag: restored,
            from: position_to_option_hash_resolved(ctx.txn(), source, None),
            to: GraphNode {
                change: ctx.txn().get_external(target.change)?,
                start: target.start,
                end: target.end,
            },
            introduced_by: Some(deleting_hash),
        };
        if !edges.contains(&edge) {
            edges.push(edge);
        }
    }

    edges.sort_by_key(|edge| {
        (
            edge.to.change,
            edge.to.start,
            edge.to.end,
            edge.from.change,
            edge.from.pos,
        )
    });
    Ok(EdgeUpdate {
        edges,
        inode: position_to_option_hash_resolved(ctx.txn(), inode_position, None),
    })
}

fn root_directory_anchor() -> Position<Option<Hash>> {
    Position {
        change: Some(Hash::NONE),
        pos: ChangePosition::ROOT,
    }
}

fn ensure_parent_directory_anchor<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: &str,
    child_path: &str,
    result: &mut GlobalizedFile,
) -> GlobalizeResult<Position<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    let parent = extract_parent(path);
    if parent.is_empty() {
        Ok(root_directory_anchor())
    } else {
        ensure_directory_anchor(ctx, parent, child_path, result)
    }
}

fn ensure_directory_anchor<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    directory_path: &str,
    child_path: &str,
    result: &mut GlobalizedFile,
) -> GlobalizeResult<Position<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps,
{
    if let Some(anchor) = ctx.directory_anchor(directory_path) {
        return Ok(anchor);
    }

    if let Some(inode) = ctx.txn().get_inode(directory_path)? {
        if let Some(position) = ctx.txn().inode_position(inode)? {
            let hash = ctx.txn().get_external(position.change)?.ok_or(
                GlobalizeError::MissingExternalHash {
                    node_id: position.change,
                },
            )?;

            // TREE is a global projection. An entry introduced only on a sibling
            // view is absent from this filtered graph and must not become a causal
            // dependency of the current view.
            if ctx.txn().is_change_visible(position.change) {
                if !ctx.txn().is_directory(inode)? {
                    return Err(GlobalizeError::ParentNotDirectory {
                        path: child_path.to_string(),
                        parent: directory_path.to_string(),
                    });
                }

                let inode_node = GraphNode::new(position.change, position.pos, position.pos);
                if !ctx.txn().has_vertex(inode_node)? {
                    return Err(GlobalizeError::InvalidParentMetadata {
                        path: child_path.to_string(),
                        parent: directory_path.to_string(),
                        reason: "directory inode anchor is missing from the graph",
                    });
                }

                let anchor = Position {
                    change: Some(hash),
                    pos: position.pos,
                };
                ctx.add_dependency(hash);
                ctx.register_directory_anchor(directory_path, anchor);
                return Ok(anchor);
            }
        } else if directory_path != child_path {
            // A staged explicit directory legitimately has no graph position
            // while its own DirAdd is being assembled. A parent without one is
            // incomplete metadata and must still fail before emitting a child.
            return Err(GlobalizeError::InvalidParentMetadata {
                path: child_path.to_string(),
                parent: directory_path.to_string(),
                reason: "directory inode has no graph position",
            });
        }
    }

    let parent_anchor = ensure_parent_directory_anchor(ctx, directory_path, child_path, result)?;
    let dirname = extract_filename(directory_path);
    if dirname.is_empty() {
        return Err(GlobalizeError::InvalidParentMetadata {
            path: child_path.to_string(),
            parent: directory_path.to_string(),
            reason: "directory path has no name component",
        });
    }

    let (name_start, name_end) = ctx.append_content(dirname.as_bytes());
    let inode_anchor = Position {
        change: None,
        pos: name_end,
    };
    let name_anchor = Position {
        change: None,
        pos: name_end,
    };

    result.add_hunk(GraphOp::DirAdd {
        add_name: Insertion {
            predecessors: vec![parent_anchor],
            successors: vec![],
            flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
            start: name_start,
            end: name_end,
            inode: parent_anchor,
        },
        add_inode: Insertion {
            predecessors: vec![name_anchor],
            successors: vec![],
            flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
            start: name_end,
            end: name_end,
            inode: inode_anchor,
        },
        path: directory_path.to_string(),
    });
    ctx.register_directory_anchor(directory_path, inode_anchor);

    Ok(inode_anchor)
}

/// Return the byte slice of `content` covering `len` lines starting at line
/// `start` (0-indexed), where lines are delimited by `\n` (the newline is
/// included with the line it terminates).  Used to extract the replacement
/// slice for a Replace hunk so that `globalize_replace` only creates
/// per-line vertices for the lines it actually replaces.
fn slice_lines(content: &[u8], start: usize, len: usize) -> &[u8] {
    if len == 0 {
        return &[];
    }
    let mut line = 0usize;
    let mut byte_start: Option<usize> = None;
    let mut byte_end = content.len();
    if start == 0 {
        byte_start = Some(0);
    }
    for (i, &b) in content.iter().enumerate() {
        if b == b'\n' {
            line += 1;
            if byte_start.is_none() && line == start {
                byte_start = Some(i + 1);
            }
            if byte_start.is_some() && line == start + len {
                byte_end = i + 1;
                break;
            }
        }
    }
    match byte_start {
        Some(s) if s <= byte_end => &content[s..byte_end],
        _ => &[],
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{FileOps, LineOps};
    use crate::crdt::{BranchId, TrunkId};

    #[test]
    fn move_file_ops_retain_trunk_op_during_content_range_enrichment() {
        let change = NodeId::new(9);
        let trunk = TrunkId::new(change, 0);
        let mut file_ops = FileOps::move_file(trunk, "old.rs".into(), "new.rs".into());
        file_ops.add_line_op(LineOps::insert_at(
            BranchId::new(change, 1),
            None,
            Vec::new(),
            2,
        ));
        let range = HunkContentRange {
            kind: BuiltHunkKind::Insert,
            new_start: 1,
            new_len: 1,
            content_start: ChangePosition::new(50),
            content_end: ChangePosition::new(54),
            uses_full_content: false,
        };

        enrich_file_ops_for_edit(&mut file_ops, b"alpha\nnew\nomega\n", &[range]);

        assert!(file_ops.is_move());
        assert_eq!(
            file_ops.line_ops()[0].content_range(),
            Some((ChangePosition::new(50), ChangePosition::new(54)))
        );
    }
}
