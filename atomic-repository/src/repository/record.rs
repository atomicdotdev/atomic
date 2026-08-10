use std::sync::Arc;

use super::*;
use crate::apply::InsertOptions;
use crate::repository::collect_visible_change_ids;
use atomic_core::pristine::{CachedGraphTxn, ViewGraph};

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
        let trace_record = std::env::var_os("ATOMIC_TRACE_RECORD").is_some();
        use atomic_core::output::Memory;
        use atomic_core::record::workflow::{
            assemble_change, record_added_file, record_deleted_file, record_modified_file,
            DetectedFile, RecordedFile,
        };

        // Build the final header (may get message from options)
        let final_header = build_header(header, &options);

        // Get repository status to find modified files
        let status_t0 = std::time::Instant::now();
        let status = self
            .status(StatusOptions::default())
            .map_err(RecordError::Repository)?;
        if trace_record {
            eprintln!(
                "[record] status: {:.1}ms ({} entries)",
                status_t0.elapsed().as_secs_f64() * 1000.0,
                status.entries().len(),
            );
        }

        log::debug!(
            "record: status returned {} entries (modified={}, added={}, deleted={}, clean={}, untracked={})",
            status.entries().len(),
            status.modified_count(),
            status.added_count(),
            status.deleted_count(),
            status.entries().iter().filter(|e| e.status() == FileStatus::Clean).count(),
            status.untracked_count(),
        );

        // Refuse to record files that still contain conflict markers, so an
        // unresolved merge is never baked into history. This runs over the
        // raw status (before the recordable filter) so it also catches files
        // reported as Conflicted. Overridable via
        // `RecordOptions::allow_conflict_markers`.
        if !options.get_allow_conflict_markers() {
            for entry in status.entries() {
                let is_directory = entry.details().map(|d| d == "directory").unwrap_or(false);
                if is_directory {
                    continue;
                }
                if !matches!(
                    entry.status(),
                    FileStatus::Added | FileStatus::Modified | FileStatus::Conflicted
                ) {
                    continue;
                }
                let path = entry.path().to_string_lossy().to_string();
                if !options.should_include(&path) {
                    continue;
                }
                let full_path = self.root.join(&path);
                if let Ok(content) = std::fs::read(&full_path) {
                    if let Some(line) = super::materialize::first_conflict_marker_line(&content) {
                        return Err(RecordError::ConflictMarkersPresent { path, line });
                    }
                }
            }
        }

        // Filter to recordable files. When conflict markers are explicitly
        // allowed, remap Conflicted entries to Modified so they remain
        // recordable (the file's underlying change is a modification).
        let remapped_entries: Vec<FileStatusEntry>;
        let entries_for_filter: &[FileStatusEntry] = if options.get_allow_conflict_markers() {
            remapped_entries = status
                .entries()
                .iter()
                .map(|e| {
                    if e.status() == FileStatus::Conflicted {
                        let mut m =
                            FileStatusEntry::new(e.path().to_path_buf(), FileStatus::Modified);
                        if let Some(inode) = e.inode() {
                            m.set_inode(inode);
                        }
                        m
                    } else {
                        e.clone()
                    }
                })
                .collect();
            &remapped_entries
        } else {
            status.entries()
        };
        let files_to_record = filter_files(entries_for_filter, &options);

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

        // ── Rename detection (Stage 1: git-style raw rename) ──────────────
        //
        // A tracked path now missing from disk (Deleted) whose byte-identical
        // content reappears at an untracked on-disk path (Untracked) is a
        // RENAME, not a delete+add. Pair them and emit a single
        // `GraphOp::FileMove` that reuses the ORIGINAL inode (preserving
        // history/blame), instead of a FileDel + FileAdd that would allocate a
        // fresh inode and lose the connection.
        //
        // The old path is still in TREE (a raw `fs::rename` never touches
        // tracking), so globalize's FileMove emitter can resolve the inode via
        // `get_inode(old_path)` unchanged. The `atomic mv` command eagerly
        // rewrites TREE and is therefore NOT covered here — that is Stage 2
        // (see docs/MERGE-CONFLICT-RUBRIC.md §6.7).
        //
        // `renamed_from` collects the old (Deleted) paths that were reclassified
        // as moves so the main loop skips deleting them.
        let mut renamed_from: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut rename_moves: Vec<(String, String, atomic_core::types::Inode)> = Vec::new();
        {
            // Candidate destinations: untracked files present on disk.
            let mut untracked: Vec<(String, Vec<u8>)> = Vec::new();
            for e in status.entries() {
                if e.status() != FileStatus::Untracked {
                    continue;
                }
                let p = e.path().to_string_lossy().to_string();
                if !options.should_include(&p) {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(self.root.join(&p)) {
                    untracked.push((p, bytes));
                }
            }
            if !untracked.is_empty() {
                let mut used_new: std::collections::HashSet<usize> =
                    std::collections::HashSet::new();
                for f in &files_to_record {
                    if f.status() != FileStatus::Deleted {
                        continue;
                    }
                    if f.details().map(|d| d == "directory").unwrap_or(false) {
                        continue; // directory renames are out of scope for Stage 1
                    }
                    let old_path = f.path().to_string_lossy().to_string();
                    // The deleted file's content as the graph still holds it.
                    let old_bytes = match self.get_file_content(&old_path) {
                        Ok(Some(b)) => b,
                        _ => continue,
                    };
                    // Pair with the first unused untracked file of identical bytes.
                    let matched = untracked.iter().enumerate().find(|(i, (_, nb))| {
                        !used_new.contains(i) && nb.len() == old_bytes.len() && **nb == old_bytes
                    });
                    let Some((idx, (new_path, _))) = matched else {
                        continue;
                    };
                    let new_path = new_path.clone();
                    let inode = match f.inode() {
                        Some(ino) => ino,
                        None => match self.get_file_inode(&old_path) {
                            Ok(Some(ino)) => ino,
                            _ => continue,
                        },
                    };
                    used_new.insert(idx);
                    renamed_from.insert(old_path.clone());
                    rename_moves.push((old_path, new_path, inode));
                }
            }
        }

        // Statistics tracking
        let mut stats = RecordStats::new();
        let mut recorded_files: Vec<RecordedFile> = Vec::new();
        let mut recorded_paths: Vec<String> = Vec::new();
        let mut deleted_paths: Vec<String> = Vec::new();
        let mut skipped_paths: Vec<String> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();

        // Emit a FileMove RecordedFile for each detected rename. globalize's
        // Moved branch turns each into a single `GraphOp::FileMove` (reusing the
        // original inode); write_recorded then updates TREE (old → new).
        for (old_path, new_path, inode) in &rename_moves {
            let mut recorded = RecordedFile::new(new_path);
            recorded.set_kind(atomic_core::record::workflow::DetectionKind::Moved);
            recorded.set_old_path(old_path.clone());
            recorded.set_inode(*inode);
            if let Ok(txn) = self.pristine.read_txn() {
                if let Ok(Some(pos)) = txn.inode_position(*inode) {
                    recorded.set_position(pos);
                }
            }
            stats.files_recorded += 1;
            stats.vertices_added += 1; // new name vertex
            recorded_paths.push(new_path.clone());
            recorded_files.push(recorded);
        }

        let core_options = options.to_core_options();

        // Create a memory working copy for the recording workflow
        let memory_wc = Memory::new();

        let record_t0 = std::time::Instant::now();

        use super::content::retrieve_content_with_filter_fast_with_fork_info;

        // Pre-compute the change filter and cached graph transaction ONCE
        // for the entire record operation.  Previously, get_file_content()
        // rebuilt these per file, which dominated record time.
        let shared_txn = self
            .pristine
            .read_txn()
            .map_err(|e| RecordError::Database(e.to_string()))?;
        let view_name_for_filter = options.get_view().unwrap_or(&self.current_view);
        let shared_change_filter = if let Some(view) = shared_txn
            .get_view(view_name_for_filter)
            .map_err(|e| RecordError::Database(e.to_string()))?
        {
            collect_visible_change_ids(&shared_txn, &view)?
        } else {
            std::collections::HashSet::new()
        };
        let shared_cached_txn =
            CachedGraphTxn::new(&shared_txn).map_err(|e| RecordError::Database(e.to_string()))?;

        if trace_record {
            eprintln!(
                "[record] change filter: {} visible changes",
                shared_change_filter.len()
            );
        }

        // Phase 1: process Added/Deleted sequentially, collect Modified for parallel.
        let mut modified_work: Vec<(String, std::path::PathBuf, std::time::Instant)> = Vec::new();

        for entry in &files_to_record {
            stats.files_processed += 1;

            let path = entry.path().to_string_lossy().to_string();
            let full_path = self.root.join(&path);
            let file_t0 = std::time::Instant::now();

            // Skip the old side of a detected rename: it was reclassified as a
            // FileMove above, so recording it as a Deleted here would emit a
            // spurious FileDel and drop the inode.
            if renamed_from.contains(&path) {
                continue;
            }

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
                    // Collect for parallel processing below.
                    modified_work.push((path.clone(), full_path.clone(), file_t0));
                }

                _ => {
                    skipped_paths.push(path.clone());
                    stats.files_skipped += 1;
                }
            }
        }

        // ── Phase 2: Process modified files in parallel ──────────────────
        //
        // Each modified file is diffed against its current view content (read
        // from the graph) and turned into BuiltHunks by `record_modified_file`.
        // Those hunks go through the normal globalize path in `assemble_change`,
        // which performs precise per-line vertex deletions/insertions. We do
        // NOT pre-globalize here: pre-globalization handled multi-line vertices
        // at whole-vertex granularity, which lost unchanged lines and produced
        // spurious conflicts on sequential edits.
        use atomic_core::output::FileSystem;
        use rayon::prelude::*;

        enum ModifiedResult {
            // `RecordedFile` is large; box it so the enum's variants stay
            // similarly sized (clippy::large_enum_variant).
            Recorded(String, Box<RecordedFile>),
            Skipped(String),
            Error(String, String),
        }

        let par_results: Vec<ModifiedResult> = modified_work
            .par_iter()
            .map(|(path, _full_path, _)| {
                let (file_inode, file_position) = match get_inode_position(&shared_txn, path) {
                    Ok(v) => v,
                    Err(e) => return ModifiedResult::Error(path.clone(), e),
                };

                // Old content = the file as the current view sees it in the graph.
                //
                // `had_fork_structure` is true when retrieval had to resolve a
                // fork or cyclic conflict (semantic merge / change-DAG
                // supersession) to produce this content — most concretely, a
                // fork left over from the orphan-view duplication bug.
                // That resolution isn't guaranteed to structurally match what a
                // plain, unambiguous checkout would render for the same graph
                // state, so a positional diff against it can corrupt the file.
                // We propagate the flag to `record_modified_file` via
                // `force_whole_file_replace` below instead of diffing against it.
                let (old_content, had_fork_structure) = {
                    use atomic_core::output::alive::RetrieveOptions;
                    let opts =
                        RetrieveOptions::new().with_change_filter(shared_change_filter.clone());
                    match retrieve_content_with_filter_fast_with_fork_info(
                        &shared_cached_txn,
                        &self.change_store,
                        file_inode,
                        file_position,
                        opts,
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            return ModifiedResult::Error(path.clone(), format!("retrieve: {}", e))
                        }
                    }
                };

                // Resolve the file's existing **alive** CRDT branches in file
                // order. The CRDT op recipe binds Delete/Modify ops to these
                // real BranchIds so the old-content lines get tombstoned;
                // without them the CRDT walker leaves stale lines alive after a
                // modify/delete. Empty when the file has no CRDT rows yet.
                use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
                use atomic_core::crdt::tables::{decode_trunk_id, encode_branch_id};
                use atomic_core::pristine::CrdtTxnT;
                let inode_u64 = file_inode.get();
                let existing_trunk_id = match shared_txn.get_crdt_inode_trunk(inode_u64) {
                    Ok(Some(trunk_key)) => Some(decode_trunk_id(&trunk_key)),
                    _ => None,
                };
                let existing_branches = match existing_trunk_id {
                    Some(trunk_id) => {
                        match iter_trunk_branches_in_file_order(&shared_txn, trunk_id) {
                            Ok(all) => {
                                let mut alive = Vec::with_capacity(all.len());
                                for b in all {
                                    let bk = encode_branch_id(&b);
                                    if let Ok(Some(bd)) = shared_txn.get_crdt_branch(&bk) {
                                        if bd.state.is_alive() {
                                            alive.push(b);
                                        }
                                    }
                                }
                                alive
                            }
                            Err(_) => Vec::new(),
                        }
                    }
                    None => Vec::new(),
                };

                // Reuse the CRDT-materialized baseline only when every existing
                // branch belongs to a change visible on this view. That keeps
                // single-view linear histories on the CRDT cleanup path without
                // leaking sibling-view branches into cross-view recording.
                let can_use_crdt_old_content = !existing_branches.is_empty()
                    && existing_branches
                        .iter()
                        .all(|b| shared_change_filter.contains(&b.change_id()));
                let crdt_old_content: Option<Vec<u8>> = if can_use_crdt_old_content {
                    match self.get_file_content_via_crdt(path.as_str()) {
                        Ok(Some(content)) if content == old_content => Some(content),
                        _ => None,
                    }
                } else {
                    None
                };
                let existing_branches_slice: Option<&[_]> = if existing_branches.is_empty() {
                    None
                } else {
                    Some(existing_branches.as_slice())
                };

                // Build line-level hunks + CRDT ops via the globalize-compatible
                // path. `record_modified_file` reads the new content from disk,
                // handles opaque/binary files, and emits BuiltHunks plus CRDT
                // ops; vertex resolution happens later during assembly.
                let mut detected =
                    atomic_core::record::workflow::DetectedFile::modified(path.as_str());
                detected.inode = Some(file_inode);
                detected.position = Some(file_position);
                let working_copy = FileSystem::from_root(&self.root);
                let file_options = if had_fork_structure {
                    log::warn!(
                        "record: '{}' had fork/cyclic conflict structure in its graph — \
                         forcing a whole-file replace instead of a positional diff \
                         (whole-file-replace safety fallback; expected to be rare and self-healing)",
                        path
                    );
                    core_options.clone().force_whole_file_replace(true)
                } else {
                    core_options.clone()
                };
                match record_modified_file(
                    &working_copy,
                    &detected,
                    &old_content,
                    crdt_old_content.as_deref(),
                    &file_options,
                    existing_trunk_id,
                    existing_branches_slice,
                ) {
                    Ok(recorded) if recorded.is_empty() => ModifiedResult::Skipped(path.clone()),
                    Ok(recorded) => ModifiedResult::Recorded(path.clone(), Box::new(recorded)),
                    Err(e) => ModifiedResult::Error(path.clone(), e),
                }
            })
            .collect();

        // Merge parallel results back into sequential state
        for result in par_results {
            match result {
                ModifiedResult::Recorded(path, recorded) => {
                    stats.files_recorded += 1;
                    stats.hunks_created += recorded.hunk_count();
                    // Account for the graph effects of a modified file so the
                    // record summary and stats-based callers see real numbers.
                    // Each Insert/Replace hunk introduces a new content span;
                    // each Delete/Replace marks existing content deleted.
                    for hunk in recorded.hunks() {
                        use atomic_core::record::workflow::graph_op::BuiltHunkKind;
                        match hunk.kind {
                            BuiltHunkKind::Insert => stats.vertices_added += 1,
                            BuiltHunkKind::Replace => {
                                stats.vertices_added += 1;
                                stats.edges_modified += 1;
                            }
                            BuiltHunkKind::Delete => stats.edges_modified += 1,
                        }
                        stats.content_bytes += hunk.content_len();
                    }
                    recorded_paths.push(path);
                    recorded_files.push(*recorded);
                }
                ModifiedResult::Skipped(path) => {
                    skipped_paths.push(path);
                    stats.files_skipped += 1;
                }
                ModifiedResult::Error(path, msg) => {
                    errors.push((path, msg));
                    stats.errors += 1;
                }
            }
        }

        if trace_record {
            eprintln!(
                "[record] file loop total: {:.1}ms ({} files, {} modified)",
                record_t0.elapsed().as_secs_f64() * 1000.0,
                files_to_record.len(),
                modified_work.len(),
            );
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
        // Wrap the read transaction in CachedGraphTxn to avoid reopening
        // the GRAPH table on every find_block/iter_adjacent call during
        // globalization. This alone eliminates ~90% of the per-vertex
        // overhead in the recording pipeline.
        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RecordError::Database(e.to_string()))?;

        let view_graph = if let Some(view) = txn
            .get_view(view_name)
            .map_err(|e| RecordError::Database(e.to_string()))?
        {
            let change_filter = collect_visible_change_ids(&txn, &view)
                .map_err(|e| RecordError::Database(e.to_string()))?;
            ViewGraph::new(&cached_txn, Arc::new(change_filter))
        } else {
            ViewGraph::new(&cached_txn, Arc::new(std::collections::HashSet::new()))
        };

        let assembly_options = options.to_assembly_options();

        let assemble_t0 = std::time::Instant::now();
        let assembly_result = assemble_change(
            &view_graph,
            &recorded_files,
            final_header,
            &assembly_options,
        )?;
        if trace_record {
            eprintln!(
                "[record] assemble_change: {:.1}ms",
                assemble_t0.elapsed().as_secs_f64() * 1000.0,
            );
        }

        let change = assembly_result.into_change();
        stats.dependency_count = change.dependencies().len();

        let serialize_t0 = std::time::Instant::now();
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

        if trace_record {
            eprintln!(
                "[record] serialize + deserialize: {:.1}ms",
                serialize_t0.elapsed().as_secs_f64() * 1000.0,
            );
        }

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
            let save_start = std::time::Instant::now();
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
            if trace_record {
                eprintln!(
                    "[record] save_change complete elapsed={:?}",
                    save_start.elapsed()
                );
            }
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
            let apply_t0 = std::time::Instant::now();
            match self.write_recorded(&outcome, apply_opts) {
                Ok(apply_outcome) => {
                    outcome.set_applied(apply_outcome.new_state);

                    // Update file index for all recorded/added files.
                    // This snapshots the filesystem metadata + content hash AFTER
                    // the record, so subsequent status() calls can skip unchanged
                    // files (mtime+size match) or avoid graph reconstruction
                    // (compare stored content hash instead).
                    if let Ok(mut idx_txn) = self.pristine.write_txn() {
                        let file_index_start = std::time::Instant::now();
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

                        // Also update FILE_INDEX for skipped files.
                        //
                        // Files are skipped when their graph content already
                        // matches the working copy (old == new). Without this,
                        // files missing a FILE_INDEX entry are perpetually
                        // reported as Modified by status (conservative mtime
                        // check) and perpetually skipped by record (content
                        // unchanged) — an infinite loop.
                        for path_str in outcome.skipped_files() {
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

                        // Remove FILE_INDEX entries for deleted files so
                        // they don't linger as stale entries.
                        for path_str in outcome.deleted_files() {
                            let _ = idx_txn.del_file_index(path_str);
                        }

                        // Clear persisted conflict state for recorded files:
                        // recording without markers IS the resolution.
                        let record_view =
                            options.get_view().unwrap_or(&self.current_view).to_string();
                        if let Ok(Some(view)) = idx_txn.get_view(&record_view) {
                            for path_str in outcome.recorded_files() {
                                let clean_path =
                                    path_str.strip_suffix("/ (directory)").unwrap_or(path_str);
                                if let Ok(Some(inode)) = idx_txn.get_inode(clean_path) {
                                    let _ = idx_txn.del_conflicts(view.id, inode.get());
                                }
                            }
                        }

                        let _ = idx_txn.commit();
                        if trace_record {
                            eprintln!(
                                "[record] file_index_update complete elapsed={:?}",
                                file_index_start.elapsed()
                            );
                        }
                    }
                }
                Err(e) => {
                    outcome.add_error("apply".to_string(), e.to_string());
                }
            }
            if trace_record {
                eprintln!(
                    "[record] write_recorded (apply): {:.1}ms",
                    apply_t0.elapsed().as_secs_f64() * 1000.0,
                );
            }
        }

        // Deflate vault working copy changes (if vault is initialized)
        if options.get_sync_vault() && self.has_vault().unwrap_or(false) {
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

        // Auto-update the enrich database (KG + content search index) with the
        // new change (best-effort). The content index step is a no-op unless an
        // index already exists, so this maintains — never builds — it.
        if options.get_enrich_kg() && outcome.was_saved() {
            let hash = *outcome.hash();
            if let Err(e) = self.kg_enrich_change(&hash) {
                log::debug!("KG enrich for change: {}", e);
            }
            if let Err(e) = crate::content_search::update_content_index_paths(
                self.root(),
                outcome.recorded_files(),
            ) {
                log::debug!("content index update for change: {}", e);
            }
        }

        if trace_record {
            eprintln!(
                "[record] TOTAL: {:.1}ms",
                record_t0.elapsed().as_secs_f64() * 1000.0,
            );
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

/// Look up inode + position for a path using a shared read transaction.
fn get_inode_position(
    txn: &atomic_core::pristine::ReadTxn,
    path: &str,
) -> Result<(Inode, Position<NodeId>), String> {
    use atomic_core::pristine::TreeTxnT;
    let inode = txn
        .get_inode(path)
        .map_err(|e| format!("get_inode: {}", e))?
        .ok_or_else(|| format!("Inode not found for {}", path))?;
    let position = txn
        .inode_position(inode)
        .map_err(|e| format!("inode_position: {}", e))?
        .ok_or_else(|| format!("Position not found for {}", path))?;
    Ok((inode, position))
}
