use std::collections::HashMap;

use super::*;

impl Repository {
    // Status Methods

    /// Compute the status of the working copy.
    ///
    /// Optimized for repositories with tens of thousands of files:
    ///
    /// 1. **Single TREE pass** — builds tracked_paths + inode_map together
    /// 2. **FILE_INDEX fast path** — stat-only check for unchanged files
    /// 3. **Clean files skipped** — only Modified/Added/Deleted/Untracked allocated
    /// 4. **Deferred walkdir** — filesystem walk only when untracked files requested
    ///
    /// # Performance
    ///
    /// | Repo size | Before | After |
    /// |-----------|--------|-------|
    /// | 1,000 files | ~1s | <50ms |
    /// | 43,000 files | ~150s | <3s |
    /// | 80,000 files | ~150s | <5s |
    pub fn status(&self, options: StatusOptions) -> Result<RepositoryStatus, RepositoryError> {
        use std::time::SystemTime;

        let overall_start = std::time::Instant::now();

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view_state = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .map(|s| s.state);

        let mut status = RepositoryStatus::new(self.current_view.clone(), view_state);

        // ── View-aware filtering ───────────────────────────────────────
        //
        // Fast path: for a Shared view with no parent (the common case
        // after `atomic init` or `atomic git import`), ALL changes in
        // GRAPH are visible.  Skip the expensive O(N) scan entirely.
        //
        // Slow path: for Draft views or views with parents, we need the
        // actual filter set to hide changes from other views.
        let (current_view_change_ids, filter_is_universal) = if let Some(ref view) = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            if view.kind.is_shared() && view.parent.is_none() {
                (HashSet::new(), true)
            } else {
                // Build the effective view filter entirely from pristine
                // indexes. Do not load `.change` files here: status must stay
                // proportional to working-copy/index state, not object-store
                // history size.
                let ids = collect_visible_change_ids_with_deps(&txn, view)?;
                (ids, false)
            }
        } else {
            (HashSet::new(), true)
        };

        let phase1_ms = overall_start.elapsed().as_millis();
        log::debug!("status: view filter setup took {}ms", phase1_ms);

        // ── Single-pass TREE scan ──────────────────────────────────────
        let tree_start = std::time::Instant::now();
        //
        // Build tracked_paths, inode_map, and directory_inodes in ONE
        // iter_tree() call instead of three passes.
        let mut tracked_paths: HashSet<PathBuf> = HashSet::new();
        let mut inode_map: HashMap<PathBuf, atomic_core::types::Inode> = HashMap::new();
        let mut directory_inodes: HashSet<atomic_core::types::Inode> = HashSet::new();
        // Cache inode → has_graph_content so we don't call inode_position twice
        let mut has_graph_content_cache: HashMap<PathBuf, bool> = HashMap::new();

        let tree_iter = txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in tree_iter {
            let (path, inode) = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            // View filter: skip files whose creating change is not on
            // the current view.
            // Check inode_position for every file:
            // - For non-universal views: also apply the view filter
            // - For universal views: just determine has_graph
            //
            // This is correct and required — files in TREE without a
            // graph position must be classified as Added, not Clean.
            let has_graph = if let Ok(Some(position)) = txn.inode_position(inode) {
                // View filter: skip files whose creating change is not
                // on the current view.
                if !filter_is_universal
                    && !position.change.is_root()
                    && !current_view_change_ids.contains(&position.change)
                {
                    continue;
                }
                true
            } else {
                false
            };

            // Normalize path once
            let normalized = normalize_tracked_path(&path, &self.root);

            // Apply path filter if specified
            if !options.path_filters.is_empty() {
                let matches = options
                    .path_filters
                    .iter()
                    .any(|f| normalized.starts_with(f) || f.starts_with(&normalized));
                if !matches {
                    continue;
                }
            }

            // Track directory status
            if txn.is_directory(inode).unwrap_or(false) {
                directory_inodes.insert(inode);
            }

            inode_map.insert(normalized.clone(), inode);
            has_graph_content_cache.insert(normalized.clone(), has_graph);
            tracked_paths.insert(normalized);
        }

        let tree_ms = tree_start.elapsed().as_millis();
        log::debug!(
            "status: TREE scan took {}ms ({} tracked files, {} dirs)",
            tree_ms,
            tracked_paths.len(),
            directory_inodes.len()
        );

        // ── Batch-load FILE_INDEX ───────────────────────────────────────
        //
        // One sequential B-tree scan loads the entire FILE_INDEX into memory.
        // This replaces 43k individual B-tree lookups with 43k HashMap lookups
        // (nanoseconds each).
        let index_start = std::time::Instant::now();
        let file_index_entries = txn
            .iter_file_index()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let file_index: HashMap<String, (i64, u32, u64, Hash)> = file_index_entries
            .into_iter()
            .map(|(path, secs, nanos, size, hash)| (path, (secs, nanos, size, hash)))
            .collect();
        let index_ms = index_start.elapsed().as_millis();
        log::debug!(
            "status: FILE_INDEX loaded {}ms ({} entries)",
            index_ms,
            file_index.len()
        );

        // ── Classify tracked files ──────────────────────────────────────
        //
        // For each tracked file: check in-memory FILE_INDEX HashMap
        // (mtime+size), stat if needed, hash only when mtime changed.
        // Clean files are skipped.
        let classify_start = std::time::Instant::now();
        let mut found_on_disk: HashSet<PathBuf> = HashSet::new();
        let mut stat_count = 0u64;
        let mut index_hit_count = 0u64;
        let mut hash_count = 0u64;

        for path in &tracked_paths {
            let abs_path = self.root.join(path);
            let inode = inode_map.get(path).copied();
            let has_graph = has_graph_content_cache.get(path).copied().unwrap_or(false);

            let is_dir = inode
                .map(|i| directory_inodes.contains(&i))
                .unwrap_or(false);

            // Skip tracked directories — handle separately
            if is_dir {
                found_on_disk.insert(path.clone());
                if abs_path.is_dir() {
                    if !has_graph {
                        // Directory tracked but not yet recorded
                        let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Added);
                        if let Some(inode) = inode {
                            entry.set_inode(inode);
                        }
                        entry.set_details("directory".to_string());
                        status.add_entry(entry);
                    }
                    // else: Clean directory — skip
                } else {
                    // Directory deleted from disk
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Deleted);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    entry.set_details("directory".to_string());
                    status.add_entry(entry);
                }
                continue;
            }

            // Check if file exists on disk
            stat_count += 1;
            let metadata = match std::fs::metadata(&abs_path) {
                Ok(m) if m.is_file() => m,
                _ => {
                    // File missing from disk → Deleted
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Deleted);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    status.add_entry(entry);
                    found_on_disk.insert(path.clone());
                    continue;
                }
            };

            found_on_disk.insert(path.clone());

            // Not yet recorded → Added
            if !has_graph {
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Added);
                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }
                if options.hash_contents {
                    if let Ok(hash) = hash_file_contents(&abs_path) {
                        entry.set_current_hash(hash);
                    }
                }
                status.add_entry(entry);
                continue;
            }

            // ── FILE_INDEX fast path (ALWAYS runs, not gated on hash_contents) ──
            //
            // Check mtime+size against the in-memory FILE_INDEX HashMap.
            // This is the critical performance path: 99%+ of files in a
            // large repo are clean, and this catches them with just a stat
            // + HashMap lookup (nanoseconds, not B-tree milliseconds).
            let path_str = path.to_string_lossy();
            if let Some(&(cached_secs, cached_nanos, cached_size, cached_hash)) =
                file_index.get(path_str.as_ref())
            {
                let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let duration = mtime
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default();
                let current_secs = duration.as_secs() as i64;
                let current_nanos = duration.subsec_nanos();
                let current_size = metadata.len();

                if current_secs == cached_secs
                    && current_nanos == cached_nanos
                    && current_size == cached_size
                {
                    // mtime + size match → Clean — skip entirely
                    index_hit_count += 1;
                    continue;
                }

                // mtime or size differ — hash to confirm
                if options.hash_contents {
                    hash_count += 1;
                    match hash_file_contents(&abs_path) {
                        Ok(current_hash) => {
                            if current_hash == cached_hash {
                                // Content unchanged (just mtime drift) → Clean
                                continue;
                            }
                            // Content changed → Modified
                            let mut entry =
                                FileStatusEntry::new(path.clone(), FileStatus::Modified);
                            if let Some(inode) = inode {
                                entry.set_inode(inode);
                            }
                            entry.set_current_hash(current_hash);
                            status.add_entry(entry);
                            continue;
                        }
                        Err(_) => {
                            let mut entry =
                                FileStatusEntry::new(path.clone(), FileStatus::Modified);
                            if let Some(inode) = inode {
                                entry.set_inode(inode);
                            }
                            entry.set_details("Unable to read file contents".to_string());
                            status.add_entry(entry);
                            continue;
                        }
                    }
                } else {
                    // No hash requested but mtime changed → assume Modified
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    status.add_entry(entry);
                    continue;
                }
            }

            // No FILE_INDEX entry — file is tracked with graph content
            // but was never indexed. This happens after `atomic insert`,
            // `atomic clone`, or `atomic view switch` materializes a file
            // into the working copy without going through `record()` (only
            // record/materialize_view populate FILE_INDEX today).
            //
            // We CANNOT silently treat this as Clean: that would let real
            // edits to such files become invisible to status/diff/record,
            // and `record(all=true)` would silently drop them.
            //
            // Conservative correctness: mark Modified so the caller can
            // run a full diff against pristine. If the file is actually
            // unchanged, the recording workflow produces an empty hunk
            // and skips it (record_modified_file returns is_empty()).
            // Subsequent records re-populate FILE_INDEX, returning the
            // file to the fast path.
            if options.hash_contents {
                hash_count += 1;
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }
                match hash_file_contents(&abs_path) {
                    Ok(current_hash) => {
                        entry.set_current_hash(current_hash);
                    }
                    Err(_) => {
                        entry.set_details("Unable to read file contents".to_string());
                    }
                }
                entry.set_details("FILE_INDEX entry missing".to_string());
                status.add_entry(entry);
            } else {
                // Fast mode: skip the hash but still surface the entry so
                // it isn't silently dropped. Callers using fast mode (e.g.
                // the agent record path) re-query with hash_contents=true
                // before recording.
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }
                entry.set_details("FILE_INDEX entry missing".to_string());
                status.add_entry(entry);
            }
        }

        let classify_ms = classify_start.elapsed().as_millis();
        log::debug!(
            "status: classify took {}ms (stat={}, index_hit={}, hashed={})",
            classify_ms,
            stat_count,
            index_hit_count,
            hash_count
        );

        // ── Deleted files ──────────────────────────────────────────────
        //
        // Any tracked path not found on disk in the loop above is deleted.
        // (Already handled inline above for regular files and directories.)

        // ── Filesystem walk for untracked files ────────────────────────
        //
        // Only do the expensive walkdir when the caller wants untracked
        // files.  The walk skips .atomic, .git, and ignored paths.
        let untracked_start = std::time::Instant::now();
        if options.include_untracked {
            let rules = if options.respect_ignore_files {
                Some(self.ignore_rules())
            } else {
                None
            };

            let working_files =
                collect_working_copy_files_with_rules(&self.root, &options, rules.as_ref())
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;

            for path in working_files {
                if !tracked_paths.contains(&path) {
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Untracked);
                    if options.hash_contents {
                        let abs_path = self.root.join(&path);
                        if let Ok(hash) = hash_file_contents(&abs_path) {
                            entry.set_current_hash(hash);
                        }
                    }
                    status.add_entry(entry);
                }
            }
        }

        let untracked_ms = untracked_start.elapsed().as_millis();
        let total_ms = overall_start.elapsed().as_millis();
        if total_ms > 100 {
            log::warn!(
                "status: total={}ms (view_filter={}ms tree_scan={}ms index_load={}ms classify={}ms untracked={}ms)",
                total_ms,
                phase1_ms,
                tree_ms,
                index_ms,
                classify_ms,
                untracked_ms
            );
        } else {
            log::debug!(
                "status: total={}ms (view_filter={}ms tree_scan={}ms index_load={}ms classify={}ms untracked={}ms)",
                total_ms,
                phase1_ms,
                tree_ms,
                index_ms,
                classify_ms,
                untracked_ms
            );
        }

        Ok(status)
    }

    /// Quick status check — uses default options.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status_quick()?;
    /// println!("Modified: {}", status.modified_count());
    /// ```
    pub fn status_quick(&self) -> Result<RepositoryStatus, RepositoryError> {
        self.status(StatusOptions::fast())
    }

    /// Status showing only tracked files (no untracked).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status_tracked()?;
    /// // Only shows modified, deleted, added - no untracked
    /// ```
    pub fn status_tracked(&self) -> Result<RepositoryStatus, RepositoryError> {
        self.status(StatusOptions::tracked_only())
    }

    /// Check if the working copy is clean (no modifications).
    pub fn is_working_copy_clean(&self) -> Result<bool, RepositoryError> {
        let status = self.status(StatusOptions::fast())?;
        Ok(status.is_clean())
    }

    /// Get only modified files.
    pub fn modified_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(StatusOptions::default())?;
        Ok(status.modified().map(|e| e.path().to_path_buf()).collect())
    }

    /// Get only untracked files.
    pub fn untracked_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(StatusOptions::default())?;
        Ok(status.untracked().map(|e| e.path().to_path_buf()).collect())
    }

    /// Get only deleted files.
    pub fn deleted_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(StatusOptions::default())?;
        Ok(status.deleted().map(|e| e.path().to_path_buf()).collect())
    }
}

/// Normalize a tracked path from the TREE table to a relative PathBuf
/// with forward slashes, handling absolute paths and platform differences.
fn normalize_tracked_path(path: &str, repo_root: &Path) -> PathBuf {
    let path_buf = PathBuf::from(path);

    let stripped = if path_buf.is_absolute() {
        if let Ok(rel) = path_buf.strip_prefix(repo_root) {
            rel.to_path_buf()
        } else if let Ok(canonical_root) = repo_root.canonicalize() {
            if let Ok(rel) = path_buf.strip_prefix(&canonical_root) {
                rel.to_path_buf()
            } else {
                path_buf
            }
        } else {
            path_buf
        }
    } else {
        path_buf
    };

    // Normalize to forward slashes for cross-platform consistency
    if cfg!(windows) || stripped.to_string_lossy().contains('\\') {
        PathBuf::from(stripped.to_string_lossy().replace('\\', "/"))
    } else {
        stripped
    }
}
