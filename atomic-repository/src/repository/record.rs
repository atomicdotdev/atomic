use std::sync::Arc;

use super::*;
use crate::apply::InsertOptions;
use crate::repository::collect_visible_change_ids;
use atomic_core::pristine::ViewGraph;

impl Repository {
    /// This is the main entry point for creating a change from working copy
    /// modifications. It detects changes, creates hunks, globalizes positions,
    /// and assembles a complete change.
    ///
    /// # Arguments
    ///
    /// * `header` - The change header (message, author, etc.)
    /// * `options` - Options controlling recording behavior
    ///
    /// # Returns
    ///
    /// A `RecordOutcome` containing the recorded change, hash, and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No changes are detected (working copy is clean)
    /// - A file cannot be read
    /// - Globalization fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::{Repository, RecordOptions};
    /// use atomic_core::change::{Author, ChangeHeader};
    ///
    /// let repo = Repository::open(".")?;
    ///
    /// let header = ChangeHeader::builder()
    ///     .message("Add new feature")
    ///     .author(Author::new("Alice", Some("alice@example.com")))
    ///     .build();
    ///
    /// let result = repo.record(header, RecordOptions::default())?;
    /// println!("Created change: {}", result.hash().to_base32());
    /// ```
    pub fn record(
        &self,
        header: ChangeHeader,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        use atomic_core::output::Memory;
        use atomic_core::record::workflow::{
            assemble_change, record_added_file, record_deleted_file, record_modified_file,
            DetectedFile, RecordedFile,
        };

        // Build the final header (may get message from options)
        let final_header = build_header(header, &options);

        // Get repository status to find modified files
        let status = self
            .status(StatusOptions::default())
            .map_err(RecordError::Repository)?;

        log::debug!(
            "record: status returned {} entries (modified={}, added={}, deleted={}, clean={}, untracked={})",
            status.entries().len(),
            status.modified_count(),
            status.added_count(),
            status.deleted_count(),
            status.entries().iter().filter(|e| e.status() == FileStatus::Clean).count(),
            status.untracked_count(),
        );

        // Filter to recordable files
        let files_to_record = filter_files(status.entries(), &options);

        log::debug!(
            "record: filter_files returned {} recordable files",
            files_to_record.len(),
        );
        for f in &files_to_record {
            log::debug!("record:   {:?} {}", f.status(), f.path().display(),);
        }

        if files_to_record.is_empty() {
            return Err(RecordError::NothingToRecord);
        }

        // Statistics tracking
        let mut stats = RecordStats::new();
        let mut recorded_files: Vec<RecordedFile> = Vec::new();
        let mut recorded_paths: Vec<String> = Vec::new();
        let mut deleted_paths: Vec<String> = Vec::new();
        let mut skipped_paths: Vec<String> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();

        let core_options = options.to_core_options();

        // Create a memory working copy for the recording workflow
        let memory_wc = Memory::new();

        // Process each file
        for entry in &files_to_record {
            stats.files_processed += 1;

            let path = entry.path().to_string_lossy().to_string();
            let full_path = self.root.join(&path);

            // Check if this is a directory (from the details field)
            let is_directory = entry.details().map(|d| d == "directory").unwrap_or(false);

            match entry.status() {
                FileStatus::Added if is_directory => {
                    // Handle added directory - create DirAdd graph_op
                    // For now, directories are tracked but their hunks will be
                    // generated during globalization. We just need to record
                    // that this directory was added.
                    stats.directories_recorded += 1;
                    stats.vertices_added += 2; // name span + inode span
                    recorded_paths.push(format!("{}/ (directory)", path));

                    // Create a minimal RecordedFile for the directory
                    // The actual GraphOp::DirAdd will be created during globalization
                    let recorded = RecordedFile::new_directory(&path);
                    recorded_files.push(recorded);
                }

                FileStatus::Added => {
                    // Read file content
                    log::debug!(
                        "record: processing Added file '{}' (full_path={})",
                        path,
                        full_path.display()
                    );
                    match std::fs::read(&full_path) {
                        Ok(content) => {
                            log::debug!("record: read {} bytes from '{}'", content.len(), path);
                            // Check size limit
                            if content.len() as u64 > options.max_file_size() {
                                if options.skip_binary() {
                                    skipped_paths.push(path.clone());
                                    stats.files_skipped += 1;
                                    continue;
                                } else {
                                    return Err(RecordError::FileTooLarge {
                                        path: path.clone(),
                                        size: content.len() as u64,
                                        limit: options.max_file_size(),
                                    });
                                }
                            }

                            // Write to memory working copy
                            memory_wc.add_file(&path, &content);

                            // Create a detected file descriptor
                            let detected = DetectedFile::added(&path);

                            // Record the added file
                            log::debug!("record: calling record_added_file for '{}'", path);
                            match record_added_file(&memory_wc, &detected, &core_options) {
                                Ok(recorded) => {
                                    log::debug!(
                                        "record: record_added_file '{}' returned: is_empty={} hunks={} content_len={}",
                                        path, recorded.is_empty(), recorded.hunk_count(), recorded.content_len()
                                    );
                                    if !recorded.is_empty() {
                                        stats.files_recorded += 1;
                                        stats.hunks_created += recorded.hunk_count();
                                        stats.content_bytes += recorded.content_len() as u64;
                                        // FileAdd creates 3 vertices: name, inode, content
                                        stats.vertices_added += 3;

                                        // Collect CRDT token-level statistics
                                        if let Some(crdt_stats) = recorded.crdt_stats() {
                                            stats.lines_added += crdt_stats.lines_added;
                                            stats.lines_deleted += crdt_stats.lines_deleted;
                                            stats.lines_modified += crdt_stats.lines_modified;
                                            stats.tokens_added += crdt_stats.tokens_added;
                                            stats.tokens_deleted += crdt_stats.tokens_deleted;
                                            stats.tokens_replaced += crdt_stats.tokens_replaced;
                                        }

                                        recorded_paths.push(path.clone());
                                        recorded_files.push(recorded);
                                    } else {
                                        log::debug!(
                                            "record: '{}' produced empty result, skipping",
                                            path
                                        );
                                        skipped_paths.push(path.clone());
                                        stats.files_skipped += 1;
                                    }
                                }
                                Err(e) => {
                                    log::debug!(
                                        "record: record_added_file '{}' error: {:?}",
                                        path,
                                        e
                                    );
                                    errors.push((path.clone(), format!("{:?}", e)));
                                    stats.errors += 1;
                                }
                            }
                        }
                        Err(e) => {
                            log::debug!("record: failed to read '{}': {}", path, e);
                            errors.push((path.clone(), e.to_string()));
                            stats.errors += 1;
                        }
                    }
                }

                FileStatus::Deleted if is_directory => {
                    // Handle deleted directory - create DirDel graph_op
                    // Look up the directory's inode
                    let txn = self
                        .pristine
                        .read_txn()
                        .map_err(|e| RecordError::Database(e.to_string()))?;

                    let inode = match txn.get_inode(&path) {
                        Ok(Some(inode)) => inode,
                        Ok(None) => {
                            errors.push((path.clone(), "Directory inode not found".to_string()));
                            stats.errors += 1;
                            continue;
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("Failed to get inode: {}", e)));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    // Verify it's actually a directory
                    if !txn.is_directory(inode).unwrap_or(false) {
                        errors.push((path.clone(), "Path is not a directory".to_string()));
                        stats.errors += 1;
                        continue;
                    }

                    // Get the position for this directory's inode
                    let position = match txn.inode_position(inode) {
                        Ok(Some(pos)) => pos,
                        Ok(None) => {
                            errors.push((path.clone(), "Directory position not found".to_string()));
                            stats.errors += 1;
                            continue;
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("Failed to get position: {}", e)));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    stats.directories_recorded += 1;
                    stats.edges_modified += 1; // deletion edge
                                               // Store the actual path for tree deletion, not the display format
                    deleted_paths.push(path.clone());

                    // Create a RecordedFile for the deleted directory with inode and position
                    let mut recorded = RecordedFile::new_deleted_directory(&path);
                    recorded.set_inode(inode);
                    recorded.set_position(position);
                    recorded_files.push(recorded);
                }

                FileStatus::Deleted => {
                    // For deleted files, we need to look up the inode and position
                    // from the pristine so that globalization can find the content
                    // vertices to mark as deleted.
                    let (file_inode, file_position) = {
                        let txn = self
                            .pristine
                            .read_txn()
                            .map_err(|e| RecordError::Database(e.to_string()))?;

                        // Get the inode for this path
                        let inode = match txn.get_inode(&path) {
                            Ok(Some(inode)) => inode,
                            Ok(None) => {
                                // No inode found - file was never recorded
                                errors.push((
                                    path.clone(),
                                    "File inode not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors.push((path.clone(), format!("Failed to get inode: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        // Get the graph position for this inode
                        let position = match txn.inode_position(inode) {
                            Ok(Some(pos)) => pos,
                            Ok(None) => {
                                errors.push((
                                    path.clone(),
                                    "File position not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors
                                    .push((path.clone(), format!("Failed to get position: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        (inode, position)
                    };

                    // Create a detected file descriptor for deletion with inode/position
                    let mut detected = DetectedFile::deleted(&path);
                    detected.inode = Some(file_inode);
                    detected.position = Some(file_position);

                    // Record deletion (no content needed)
                    match record_deleted_file(&detected, &core_options) {
                        Ok(recorded) => {
                            stats.files_recorded += 1;
                            stats.hunks_created += recorded.hunk_count();
                            // FileDel creates EdgeUpdate atoms to mark edges as deleted
                            stats.edges_modified += 1;

                            // Collect CRDT token-level statistics
                            if let Some(crdt_stats) = recorded.crdt_stats() {
                                stats.lines_added += crdt_stats.lines_added;
                                stats.lines_deleted += crdt_stats.lines_deleted;
                                stats.lines_modified += crdt_stats.lines_modified;
                                stats.tokens_added += crdt_stats.tokens_added;
                                stats.tokens_deleted += crdt_stats.tokens_deleted;
                                stats.tokens_replaced += crdt_stats.tokens_replaced;
                            }

                            // Track this as a deleted file
                            deleted_paths.push(path.clone());
                            recorded_paths.push(path.clone());
                            recorded_files.push(recorded);
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("{:?}", e)));
                            stats.errors += 1;
                        }
                    }
                }

                FileStatus::Modified => {
                    // For modified files, we need to:
                    // 1. Look up the file's inode and graph position
                    // 2. Retrieve the old content from the graph
                    // 3. Read the new content from the working copy
                    // 4. Diff old vs new to create Edit/Replacement hunks
                    //
                    // This creates efficient incremental changes rather than
                    // replacing the entire file content.

                    // Step 1: Look up the file's inode and position from the pristine
                    // This is required for globalization to create Edit hunks instead of FileAdd
                    let (file_inode, file_position) = {
                        let txn = self
                            .pristine
                            .read_txn()
                            .map_err(|e| RecordError::Database(e.to_string()))?;

                        // Get the inode for this path
                        let inode = match txn.get_inode(&path) {
                            Ok(Some(inode)) => inode,
                            Ok(None) => {
                                // No inode found - file is tracked but not in TREE table
                                // This shouldn't happen for Modified status, but fall back
                                errors.push((
                                    path.clone(),
                                    "File inode not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors.push((path.clone(), format!("Failed to get inode: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        // Get the graph position for this inode
                        let position = match txn.inode_position(inode) {
                            Ok(Some(pos)) => pos,
                            Ok(None) => {
                                // No position found - file has inode but no graph entry
                                errors.push((
                                    path.clone(),
                                    "File position not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors
                                    .push((path.clone(), format!("Failed to get position: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        (inode, position)
                    };

                    // Step 2: Retrieve old content from the graph.
                    // Use get_file_content which builds a change filter
                    // so that the view's content is correctly scoped.
                    let old_content = match self.get_file_content(entry.path()) {
                        Ok(Some(content)) => content,
                        Ok(None) => {
                            // No recorded content found - treat as new file
                            // This can happen if the file was tracked but never recorded
                            Vec::new()
                        }
                        Err(e) => {
                            // Error retrieving content - log and skip
                            errors.push((
                                path.clone(),
                                format!("Failed to retrieve old content: {}", e),
                            ));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    // Step 2: Read new content from working copy
                    let new_content = match std::fs::read(&full_path) {
                        Ok(content) => content,
                        Err(e) => {
                            errors.push((path.clone(), e.to_string()));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    // Step 3: Check if content actually changed
                    if old_content == new_content {
                        // No actual change - skip
                        skipped_paths.push(path.clone());
                        stats.files_skipped += 1;
                        continue;
                    }

                    // Step 4: Write to memory working copy for the recording workflow
                    memory_wc.add_file(&path, &new_content);

                    // Step 5: Create a detected file descriptor for modification
                    // Include the inode and position so globalization creates Edit hunks
                    let mut detected = DetectedFile::modified(&path);
                    detected.inode = Some(file_inode);
                    detected.position = Some(file_position);

                    // Step 6: Record the modification using the diff-based workflow
                    // This creates Edit hunks for insertions and Replacement hunks
                    // for deletions, rather than a full FileAdd replacement.
                    match record_modified_file(&memory_wc, &detected, &old_content, &core_options) {
                        Ok(recorded) => {
                            if !recorded.is_empty() {
                                stats.files_recorded += 1;
                                stats.hunks_created += recorded.hunk_count();
                                stats.content_bytes += recorded.content_len() as u64;

                                // Count vertices and edges from the hunks
                                // Edit hunks create 1 span per insertion
                                // Replacement hunks create 1 span + edge modifications
                                for graph_op in recorded.hunks() {
                                    if graph_op.is_edit() {
                                        stats.vertices_added += 1;
                                    } else if graph_op.is_replace() {
                                        stats.vertices_added += 1;
                                        stats.edges_modified += 1;
                                    } else if graph_op.is_delete() {
                                        stats.edges_modified += 1;
                                    }
                                }

                                // Collect CRDT token-level statistics
                                if let Some(crdt_stats) = recorded.crdt_stats() {
                                    stats.lines_added += crdt_stats.lines_added;
                                    stats.lines_deleted += crdt_stats.lines_deleted;
                                    stats.lines_modified += crdt_stats.lines_modified;
                                    stats.tokens_added += crdt_stats.tokens_added;
                                    stats.tokens_deleted += crdt_stats.tokens_deleted;
                                    stats.tokens_replaced += crdt_stats.tokens_replaced;
                                }

                                recorded_paths.push(path.clone());
                                recorded_files.push(recorded);
                            } else {
                                // No hunks generated - content might be identical
                                skipped_paths.push(path.clone());
                                stats.files_skipped += 1;
                            }
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("{:?}", e)));
                            stats.errors += 1;
                        }
                    }
                }

                _ => {
                    // Skip other statuses
                    skipped_paths.push(path.clone());
                    stats.files_skipped += 1;
                }
            }
        }

        // Check if we actually recorded anything
        if recorded_files.is_empty() {
            return Err(RecordError::NothingToRecord);
        }

        // Assemble the change from the recorded files.
        //
        // We wrap the transaction in a ViewGraph so that the entire
        // globalization pipeline (retrieve_graph, iter_adjacent, etc.)
        // transparently sees only edges introduced by changes visible
        // on the current view.  No manual filter threading needed.
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RecordError::Database(e.to_string()))?;

        let view_name = options.get_view().unwrap_or(&self.current_view);
        let view_graph = if let Some(view) = txn
            .get_view(view_name)
            .map_err(|e| RecordError::Database(e.to_string()))?
        {
            let change_filter = collect_visible_change_ids(&txn, &view)
                .map_err(|e| RecordError::Database(e.to_string()))?;
            ViewGraph::new(&txn, Arc::new(change_filter))
        } else {
            // No view found — use an empty filter (ROOT-only visibility).
            // In practice this shouldn't happen because the repository
            // always has at least one view, but we handle it gracefully.
            ViewGraph::new(&txn, Arc::new(std::collections::HashSet::new()))
        };

        let assembly_options = options.to_assembly_options();

        let assembly_result = assemble_change(
            &view_graph,
            &recorded_files,
            final_header,
            &assembly_options,
        )?;

        let change = assembly_result.into_change();
        stats.dependency_count = change.dependencies().len();

        // Serialize to V3 format and compute content hash.
        // We keep the raw V3 bytes so we can save them directly to disk
        // without re-serializing (which would produce a different hash).
        let mut v3_bytes = Vec::new();
        let computed_hash = change
            .serialize(&mut v3_bytes)
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;

        // Reload the change from the V3 buffer to get a clean deserialized form
        let (final_change, verified_hash) = Change::deserialize(&mut v3_bytes.as_slice())
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
        debug_assert_eq!(computed_hash, verified_hash);

        let mut outcome = RecordOutcome::new(final_change, computed_hash, stats);
        // Stash the original V3 bytes so save_change can write them directly
        // instead of re-serializing (which may produce a different hash).
        outcome.set_v3_bytes(v3_bytes);

        // Add recorded/skipped/deleted files to outcome
        for path in recorded_paths {
            outcome.add_recorded_file(path);
        }
        for path in deleted_paths {
            outcome.add_deleted_file(path);
        }
        for path in skipped_paths {
            outcome.add_skipped_file(path);
        }
        for (path, error) in errors {
            outcome.add_error(path, error);
        }

        // Save to store if requested.
        // Use the original V3 bytes (not re-serialized) to ensure the hash
        // on disk matches the hash registered in the pristine graph.
        if options.get_save_to_store() {
            if let Some(v3_bytes) = outcome.v3_bytes() {
                // Fast path: write the exact V3 bytes that produced computed_hash
                self.save_change_bytes(&computed_hash, v3_bytes, outcome.change())
                    .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
            } else {
                // Fallback: re-serialize (may produce different hash — legacy path)
                self.save_change(outcome.change())
                    .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
            }
            outcome.set_saved(true);
        }

        // Apply if requested
        // We use write_recorded() instead of insert_change() because it creates
        // the TREE and INODES entries for FileAdd hunks, which is necessary
        // for the file to be recognized as tracked with graph content.
        if options.get_apply_after_record() && outcome.was_saved() {
            let apply_opts = match options.get_view() {
                Some(view) => InsertOptions::default().view(view),
                None => InsertOptions::default(),
            };
            match self.write_recorded(&outcome, apply_opts) {
                Ok(apply_outcome) => {
                    outcome.set_applied(apply_outcome.new_state);

                    // Update file index for all recorded/added files.
                    // This snapshots the filesystem metadata + content hash AFTER
                    // the record, so subsequent status() calls can skip unchanged
                    // files (mtime+size match) or avoid graph reconstruction
                    // (compare stored content hash instead).
                    if let Ok(mut idx_txn) = self.pristine.write_txn() {
                        for path_str in outcome.recorded_files() {
                            // Strip directory markers like "dir/ (directory)"
                            let clean_path =
                                path_str.strip_suffix("/ (directory)").unwrap_or(path_str);
                            let abs_path = self.root.join(clean_path);
                            if let Ok(metadata) = std::fs::metadata(&abs_path) {
                                use std::time::SystemTime;
                                let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                                let duration = mtime
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .unwrap_or_default();
                                let content_hash = std::fs::read(&abs_path)
                                    .map(|bytes| Hash::of(&bytes))
                                    .unwrap_or(Hash::ZERO);
                                let _ = idx_txn.put_file_index(
                                    clean_path,
                                    duration.as_secs() as i64,
                                    duration.subsec_nanos(),
                                    metadata.len(),
                                    &content_hash,
                                );
                            }
                        }
                        let _ = idx_txn.commit();
                    }
                }
                Err(e) => {
                    outcome.add_error("apply".to_string(), e.to_string());
                }
            }
        }

        // Deflate vault working copy changes (if vault is initialized)
        if self.has_vault().unwrap_or(false) {
            match self.vault_record_working_copy() {
                Ok(vault_paths) if !vault_paths.is_empty() => {
                    outcome.set_vault_paths(vault_paths);
                }
                Ok(_) => {} // No vault changes
                Err(e) => {
                    // Log the error but don't fail the record — vault sync
                    // is best-effort during record
                    log::warn!("Failed to sync vault working copy: {}", e);
                }
            }
        }

        // Auto-enrich KG with the new change (best-effort)
        if outcome.was_saved() {
            let hash = *outcome.hash();
            if let Err(e) = self.kg_enrich_change(&hash) {
                log::debug!("KG enrich for change: {}", e);
            }
        }

        Ok(outcome)
    }

    /// Record changes with a simple message.
    ///
    /// This is a convenience method that creates a change header with just
    /// a message.
    ///
    /// # Arguments
    ///
    /// * `message` - The change message
    /// * `options` - Recording options
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.record_with_message("Fix bug", RecordOptions::default())?;
    /// ```
    pub fn record_with_message(
        &self,
        message: impl Into<String>,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        let header = ChangeHeader::builder().message(message).build();
        self.record(header, options)
    }

    /// Fast path for external importers (e.g. git-import) that already have
    /// pre-built `RecordedFile`s and just need globalization + assembly + hashing.
    ///
    /// Returns `(Change, Hash)`.
    pub fn assemble_and_hash(
        &self,
        header: ChangeHeader,
        recorded_files: &[atomic_core::record::workflow::RecordedFile],
    ) -> Result<(Change, Hash), RecordError> {
        use atomic_core::record::workflow::assemble_change;
        use atomic_core::record::workflow::assembly::AssemblyOptions;

        let overall_start = std::time::Instant::now();

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RecordError::Database(e.to_string()))?;

        // Use a bare transaction — no ViewGraph filter needed.
        //
        // This method is the fast path for git import, where every change
        // is written to a shared view and all edges land in the global
        // GRAPH table.  A bare ReadTxn sees every edge directly.
        //
        // Wrapping in ViewGraph + collect_visible_change_ids would scan
        // the entire view change log on every call (O(N) per commit,
        // O(N²) total for N commits), which makes large imports
        // progressively slower.
        let assembly_options = AssemblyOptions::default();

        let assembly_result = assemble_change(&txn, recorded_files, header, &assembly_options)?;

        let change = assembly_result.into_change();
        log::debug!(
            "assemble_and_hash: assembly complete, content_size={} hunks={}",
            change.contents.len(),
            change.hunks().len(),
        );

        let step = std::time::Instant::now();
        let mut v3_bytes = Vec::new();
        let hash = change
            .serialize(&mut v3_bytes)
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
        let serialize_ms = step.elapsed().as_millis();

        let step = std::time::Instant::now();
        let (final_change, _) = Change::deserialize(&mut v3_bytes.as_slice())
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
        let deserialize_ms = step.elapsed().as_millis();

        let total_ms = overall_start.elapsed().as_millis();
        if serialize_ms + deserialize_ms > 100 {
            log::warn!(
                "assemble_and_hash: serialize={}ms ({} bytes) deserialize={}ms total={}ms",
                serialize_ms,
                v3_bytes.len(),
                deserialize_ms,
                total_ms,
            );
        } else {
            log::debug!(
                "assemble_and_hash: serialize={}ms ({} bytes) deserialize={}ms total={}ms",
                serialize_ms,
                v3_bytes.len(),
                deserialize_ms,
                total_ms,
            );
        }

        Ok((final_change, hash))
    }

    /// Look up a file's inode and graph position from the pristine.
    ///
    /// Returns `None` if the file is not tracked or has no graph position.
    pub fn get_inode_and_position(
        &self,
        path: &str,
    ) -> Result<Option<(Inode, Position<NodeId>)>, RepositoryError> {
        use atomic_core::pristine::TreeTxnT;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let inode = match txn
            .get_inode(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(i) => i,
            None => return Ok(None),
        };

        let position = match txn
            .inode_position(inode)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(p) => p,
            None => return Ok(None),
        };

        Ok(Some((inode, position)))
    }

    /// Record all changes with a message.
    ///
    /// This is a convenience method that records all modified files.
    ///
    /// # Arguments
    ///
    /// * `message` - The change message
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.record_all("Update all files")?;
    /// ```
    pub fn record_all(&self, message: impl Into<String>) -> Result<RecordOutcome, RecordError> {
        let options = RecordOptions::new().with_all(true);
        self.record_with_message(message, options)
    }
}
