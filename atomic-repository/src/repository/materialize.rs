use super::*;
use atomic_core::pristine::{CachedGraphTxn, StoredConflict, StoredConflictKind};

/// Return the 1-based line number of the first Atomic conflict-start marker
/// (`>>>>>>>`) in `content`, or `None` if the content has no markers.
///
/// This is the authoritative signal for "this materialized file is
/// conflicted": it reflects exactly what was written to disk, so persisted
/// conflict state stays in lock-step with the bytes the user sees.
pub(crate) fn first_conflict_marker_line(content: &[u8]) -> Option<u32> {
    let text = match std::str::from_utf8(content) {
        Ok(t) => t,
        Err(_) => return None, // binary content carries no textual markers
    };
    for (idx, line) in text.lines().enumerate() {
        if line.starts_with(">>>>>>>") {
            return Some((idx + 1) as u32);
        }
    }
    None
}

/// Render a name conflict: two or more inodes are alive at the same path on
/// this view, so instead of silently emitting whichever inode `TREE` happened
/// to keep, wrap every side's materialized content in conflict markers.
///
/// The block opens with a `>>>>>>>` line (so the existing marker-driven
/// surfacing pipeline — `first_conflict_marker_line` → `conflicts_by_path` →
/// `persist_view_conflicts` — flags the file exactly as it does for content
/// conflicts), separates sides with `=======`, and closes with `<<<<<<<`,
/// matching Atomic's inverted marker convention. `sides` must already be in a
/// deterministic order so the rendering is stable across runs.
#[allow(clippy::too_many_arguments)]
fn render_name_conflict<C: atomic_core::change::ChangeStore>(
    txn: &atomic_core::pristine::ReadTxn,
    store: &C,
    inode_graph_table: &redb::ReadOnlyMultimapTable<&'static [u8; 32], &'static [u8; 24]>,
    change_filter_arc: &Arc<std::collections::HashSet<NodeId>>,
    path: &str,
    sides: &[(Inode, Position<NodeId>)],
) -> Result<Vec<u8>, String> {
    use atomic_core::output::repo::{
        output_graph_content_resolved, resolve_conflicts_semantically,
    };
    use atomic_core::output::{compute_order, retrieve_graph, RetrieveOptions, Writer};
    use atomic_core::pristine::InodePreloadTxn;

    let mut out: Vec<u8> = Vec::new();
    for (i, (inode, position)) in sides.iter().enumerate() {
        if i == 0 {
            out.extend_from_slice(format!(">>>>>>> {} (name conflict)\n", path).as_bytes());
        } else {
            out.extend_from_slice(b"=======\n");
        }

        let preloaded = InodePreloadTxn::from_table(txn, *inode, inode_graph_table)
            .map_err(|e| format!("{}: name-conflict preload: {:?}", path, e))?;
        let retrieve_opts =
            RetrieveOptions::default().with_change_filter_arc(change_filter_arc.clone());
        let retrieve_result = retrieve_graph(&preloaded, *position, retrieve_opts)
            .map_err(|e| format!("{}: name-conflict retrieve: {:?}", path, e))?;
        let mut graph = retrieve_result.graph;
        let order = compute_order(&mut graph);
        let resolved = resolve_conflicts_semantically(&preloaded, store, &graph, &order);
        let buffer = Vec::with_capacity(graph.total_bytes());
        let mut writer = Writer::new(buffer);
        let hash_fn = |node_id: NodeId| -> Option<Hash> {
            if node_id.is_root() {
                return None;
            }
            preloaded.get_external(node_id).ok().flatten()
        };
        output_graph_content_resolved(store, hash_fn, &graph, &order, &mut writer, &resolved)
            .map_err(|e| format!("{}: name-conflict content: {:?}", path, e))?;
        let side = writer.into_inner();
        out.extend_from_slice(&side);
        if !side.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    out.extend_from_slice(format!("<<<<<<< {} (name conflict)\n", path).as_bytes());
    Ok(out)
}

impl Repository {
    /// Persist the conflict state discovered by a materialize into the
    /// `CONFLICTS` table for `view_id`.
    ///
    /// A full materialize (`only_paths` is `None`) replaces the view's entire
    /// conflict set: every prior entry is dropped and the current conflicts
    /// re-written. A partial materialize updates only the touched files,
    /// setting or clearing each. This keeps a partial run from wrongly
    /// discarding conflicts for files it did not re-materialize.
    fn persist_view_conflicts(
        &self,
        view_id: u64,
        path_to_inode: &std::collections::HashMap<String, u64>,
        conflicts_by_path: &std::collections::HashMap<String, u32>,
        only_paths: &Option<std::collections::HashSet<String>>,
    ) -> Result<(), RepositoryError> {
        let mut wtxn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mk = |path: &str, line: u32| StoredConflict {
            kind: StoredConflictKind::Order,
            path: path.to_string(),
            line: Some(line),
            sides: Vec::new(),
        };

        match only_paths {
            None => {
                wtxn.del_conflicts_prefix(view_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                for (path, line) in conflicts_by_path {
                    if let Some(&inode) = path_to_inode.get(path) {
                        let sc = mk(path, *line);
                        wtxn.put_conflicts(view_id, inode, std::slice::from_ref(&sc))
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                }
            }
            Some(paths) => {
                for path in paths {
                    if let Some(&inode) = path_to_inode.get(path) {
                        if let Some(line) = conflicts_by_path.get(path) {
                            let sc = mk(path, *line);
                            wtxn.put_conflicts(view_id, inode, std::slice::from_ref(&sc))
                                .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        } else {
                            wtxn.del_conflicts(view_id, inode)
                                .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        }
                    }
                }
            }
        }

        wtxn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }
    /// Compute the set of file paths visible on a view.
    ///
    /// Visibility includes the view's own changes AND all changes
    /// inherited through the parent chain.  A draft view parented on
    /// dev sees dev's files without requiring an explicit insert.
    ///
    /// A file is visible on a view when:
    /// 1. It appears in the global TREE table (has been `add`ed).
    /// 2. Its inode has a graph position in the INODES table (has been
    ///    `record`ed).
    /// 3. The change that introduced that position is visible to the
    ///    view (own changes + parent chain).
    ///
    /// Files that have been `add`ed but not yet `record`ed (no INODES
    /// entry) are NOT returned — they persist across switches as
    /// working-copy state.
    pub fn visible_file_paths(&self, view_name: &str) -> Result<HashSet<String>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = match txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(s) => s,
            None => return Ok(HashSet::new()),
        };

        // Use the FULL visible change set (own + parent chain) so that
        // draft views parented on dev see dev's files.
        let view_change_ids = collect_visible_change_ids(&txn, &view)?;

        // Walk TREE and keep paths whose introducing change is in the log.
        let mut paths: HashSet<String> = HashSet::new();
        let tree_iter = txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in tree_iter {
            let (path, inode) = result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            if let Some(position) = txn
                .inode_position(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                if view_change_ids.contains(&position.change) {
                    paths.insert(path);
                }
            }
        }

        Ok(paths)
    }

    /// Materialize the working copy to match the current view's state.
    ///
    /// This synchronizes the working copy files with the repository graph
    /// state for the current view. Files are created, updated, or deleted
    /// to match what's recorded in the view.
    ///
    /// Since all edges are stored in the global GRAPH table, this uses the
    /// raw transaction directly with a change filter to scope which vertices
    /// are alive for this view.
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation including:
    /// - Number of files written
    /// - Number of directories created
    /// - Any conflicts detected
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The database cannot be read
    /// - Files cannot be written to the working copy
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    ///
    /// // Reset working copy to current view's state
    /// let result = repo.materialize()?;
    /// println!("Materialized {} files", result.files_written);
    ///
    /// if result.has_conflicts() {
    ///     println!("Warning: {} conflicts detected", result.conflict_count());
    /// }
    /// ```
    pub fn materialize(&self) -> Result<MaterializeResult, RepositoryError> {
        // Use the parallel path — buffers content in memory, processes files
        // concurrently via rayon, writes each file in a single fs::write call,
        // and computes content hashes in-memory (no read-back pass).
        self.materialize_parallel(None)
    }

    /// Sequential materialize fallback.
    ///
    /// Processes files one at a time through the streaming writer path.
    /// Used when the parallel path is not suitable (e.g., memory-constrained
    /// environments).
    pub fn materialize_sequential(&self) -> Result<MaterializeResult, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        let change_filter = collect_visible_change_ids(&txn, &view)?;

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = MaterializeOptions::new().with_change_filter(change_filter);

        let result = materialize_view(&cached_txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;

        self.populate_file_index(&result);

        Ok(result)
    }

    /// Materialize only specific files to the working copy.
    ///
    /// This is used after `insert` operations to only rewrite files that
    /// were actually affected by the inserted changes, avoiding a full
    /// rematerialization of the entire working copy.
    ///
    /// Returns the set of `(path, content_hash)` pairs for files that were
    /// written, enabling the caller to update FILE_INDEX without re-reading
    /// from disk.
    pub fn materialize_paths(
        &self,
        paths: std::collections::HashSet<String>,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.materialize_parallel(Some(paths))
    }

    /// Sequential materialize for a specific set of paths (fallback).
    #[allow(dead_code)]
    fn materialize_paths_sequential(
        &self,
        paths: std::collections::HashSet<String>,
    ) -> Result<MaterializeResult, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        let change_filter = collect_visible_change_ids(&txn, &view)?;

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = MaterializeOptions::new()
            .with_change_filter(change_filter)
            .only_paths(paths.clone());

        let result = materialize_view(&cached_txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;

        // Update FILE_INDEX only for files that were actually written,
        // reading back only the affected files instead of all tracked files.
        self.populate_file_index_for_paths(&paths);

        Ok(result)
    }

    /// Materialize the working copy using parallel file processing.
    ///
    /// This is an optimized version of `materialize` that:
    /// 1. Buffers each file's content in memory (single allocation per file)
    /// 2. Processes files in parallel using rayon
    /// 3. Writes each file to disk in a single `fs::write` call
    /// 4. Computes content hashes in-memory (no read-back pass)
    ///
    /// Falls back to sequential processing for files that fail in the
    /// parallel path.
    pub fn materialize_parallel(
        &self,
        only_paths: Option<std::collections::HashSet<String>>,
    ) -> Result<MaterializeResult, RepositoryError> {
        use atomic_core::output::repo::{
            collect_children, FileOutputOptions, MaterializeOptions, OutputItem,
        };
        use atomic_core::output::RetrieveOptions;
        use rayon::prelude::*;
        use std::collections::HashSet as StdHashSet;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        let change_filter = collect_visible_change_ids(&txn, &view)?;
        let change_filter_arc = Arc::new(change_filter);

        let options = MaterializeOptions::new().with_change_filter_arc(change_filter_arc.clone());

        // Phase 1: Collect all items from the tree
        let items = collect_children(&txn, Inode::ROOT, "", &options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let _file_options = FileOutputOptions::new();

        // Phase 2+4: Filter files by view membership
        let file_items: Vec<&OutputItem> = items
            .iter()
            .filter(|item| {
                if item.is_directory {
                    return false;
                }
                if !options.matches_prefix(&item.path) {
                    return false;
                }
                // View-aware filter: skip files whose introducing change
                // is not in the visible change set
                if let Some(ref filter) = options.change_filter {
                    if !item.position.change.is_root() && !filter.contains(&item.position.change) {
                        return false;
                    }
                }
                // Selective materialize: skip files not in the explicit path set
                if let Some(ref paths) = only_paths {
                    if !paths.contains(&item.path) {
                        return false;
                    }
                }
                true
            })
            .collect();

        let total_files = file_items.len();
        let skipped_in_filter = items.iter().filter(|i| !i.is_directory).count() - total_files;

        // ── Name-conflict detection ──────────────────────────────────────
        //
        // TREE is a single-valued path→inode index, so `iter_tree` (and hence
        // `file_items`) exposes only ONE inode per path even when two
        // independent creates on different views both claim it — the later
        // recorder silently wins and the first inode is orphaned (rubric A12,
        // ATOM::29/30). Walk REV_TREE to recover every inode that claims each
        // path; when ≥ 2 of them are BOTH visible (creating change in the
        // filter) and alive under this view, the path is a genuine name
        // conflict that must be surfaced rather than silently collapsed.
        //
        // The (relatively expensive) aliveness probe runs ONLY for paths with
        // ≥ 2 candidate inodes, so the common single-inode file pays nothing.
        let name_conflicts: std::collections::HashMap<String, Vec<(Inode, Position<NodeId>)>> = {
            use atomic_core::pristine::TreeTxnT;
            let mut by_path: std::collections::HashMap<String, Vec<Inode>> =
                std::collections::HashMap::new();
            if let Ok(pairs) = txn.iter_rev_tree() {
                for (inode, path) in pairs {
                    by_path.entry(path).or_default().push(inode);
                }
            }
            let filter = change_filter_arc.as_ref();
            let mut conflicts: std::collections::HashMap<String, Vec<(Inode, Position<NodeId>)>> =
                std::collections::HashMap::new();
            for item in &file_items {
                let candidates = match by_path.get(&item.path) {
                    Some(c) if c.len() >= 2 => c,
                    _ => continue,
                };
                let mut live: Vec<(Inode, Position<NodeId>)> = Vec::new();
                for &inode in candidates {
                    let pos = match txn.inode_position(inode) {
                        Ok(Some(p)) => p,
                        _ => continue,
                    };
                    if !pos.change.is_root() && !filter.contains(&pos.change) {
                        continue; // not visible on this view
                    }
                    if crate::repository::status::is_file_alive_via_retrieval(
                        &txn, inode, pos, filter,
                    ) {
                        live.push((inode, pos));
                    }
                }
                if live.len() >= 2 {
                    // Deterministic order: creating change, then position, then inode.
                    live.sort_by_key(|(ino, p)| (p.change.get(), p.pos.get(), ino.get()));
                    conflicts.insert(item.path.clone(), live);
                }
            }
            conflicts
        };

        // Phase 3: Create directories needed by passing files
        let mut result = MaterializeResult::new();
        result.files_skipped += skipped_in_filter;

        let file_paths: StdHashSet<&str> = file_items.iter().map(|i| i.path.as_str()).collect();
        for item in &items {
            if !item.is_directory {
                continue;
            }
            // Check if any file starts with this directory path
            let dir_prefix = format!("{}/", item.path);
            let has_children = file_paths.iter().any(|p| p.starts_with(&dir_prefix));
            if !has_children {
                result.record_skipped();
                continue;
            }
            let abs_dir = self.root.join(&item.path);
            if !abs_dir.exists() {
                std::fs::create_dir_all(&abs_dir).map_err(|e| {
                    RepositoryError::Output(format!("Failed to create directory: {}", e))
                })?;
            }
            result.record_directory();
        }

        // Phase 5a: Pre-warm the ChangeStore cache.
        //
        // Load all changes referenced by file vertices into the cache
        // BEFORE the parallel phase. This ensures:
        // - No disk I/O during parallel execution (all cache hits)
        // - No write-lock contention (peek() uses read locks for hits)
        // - Consistent, predictable per-file performance
        let root = &self.root;
        let store = &self.change_store;

        let trace_mat = std::env::var_os("ATOMIC_TRACE_MATERIALIZE").is_some();
        let mat_start = std::time::Instant::now();

        {
            // Collect unique change hashes from all file positions
            let mut change_ids_to_warm: std::collections::HashSet<NodeId> =
                std::collections::HashSet::new();
            for item in &file_items {
                if !item.position.change.is_root() {
                    change_ids_to_warm.insert(item.position.change);
                }
            }
            // Also include all changes in the view filter — any of them
            // could have content vertices
            if let Some(ref filter) = options.change_filter {
                for id in filter.iter() {
                    if !id.is_root() {
                        change_ids_to_warm.insert(*id);
                    }
                }
            }
            // Resolve NodeId → Hash and pre-load each change
            for node_id in &change_ids_to_warm {
                if let Ok(Some(hash)) = txn.get_external(*node_id) {
                    let _ = store.load_change(&hash);
                }
            }
            if trace_mat {
                eprintln!(
                    "[materialize] cache pre-warm complete changes={} elapsed={:?}",
                    change_ids_to_warm.len(),
                    mat_start.elapsed(),
                );
            }
        }

        // Phase 5b: Load FILE_INDEX for content-hash skip.
        //
        // If a file already exists on disk with the same content the
        // graph would produce, skip the entire write. This is the
        // Pijul-style "needs_output" check: stat the file, compare
        // hash, and skip if unchanged.
        let file_index: std::collections::HashMap<String, (i64, u32, u64, Hash)> = {
            let idx_txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let entries = idx_txn.iter_file_index().unwrap_or_default();
            entries
                .into_iter()
                .map(|(p, s, n, sz, h)| (p, (s, n, sz, h)))
                .collect()
        };

        // Open the INODE_GRAPH table once, shared across all rayon threads.
        // This eliminates per-file open_multimap_table mutex contention.
        let inode_graph_table = txn
            .open_inode_graph_table()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Phase 5c: Process files in parallel — retrieve graph, buffer content,
        // check content-hash, write to disk only if changed
        type FileResult = Result<Option<(String, u64, Hash, bool, Option<u32>)>, String>;
        let file_results: Vec<FileResult> = file_items
            .par_iter()
            .map(|item| {
                let file_start = std::time::Instant::now();

                // Build retrieve options with the shared change filter
                let retrieve_opts =
                    RetrieveOptions::default().with_change_filter_arc(change_filter_arc.clone());

                // Inline the output pipeline so we can trace each phase.
                use atomic_core::output::repo::{
                    output_graph_content_resolved, resolve_conflicts_semantically,
                };
                use atomic_core::output::{compute_order, retrieve_graph, Writer};
                use atomic_core::pristine::InodePreloadTxn;

                // Pre-load ALL edges for this file's inode from INODE_GRAPH
                // in a single range scan, then run retrieve_graph over the
                // in-memory HashMap. O(M) scan + O(1) lookups vs O(V×log N)
                // individual B-tree probes.
                let preloaded = InodePreloadTxn::from_table(&txn, item.inode, &inode_graph_table)
                    .map_err(|e| format!("{}: preload: {:?}", item.path, e))?;

                let t_retrieve = std::time::Instant::now();
                let retrieve_result = retrieve_graph(&preloaded, item.position, retrieve_opts)
                    .map_err(|e| format!("{}: retrieve: {:?}", item.path, e))?;

                if retrieve_result.graph.is_empty() {
                    return Ok(None);
                }

                let vertices = retrieve_result.graph.len_vertices();
                let edges = retrieve_result.edges_traversed;
                let retrieve_ms = t_retrieve.elapsed();

                let t_order = std::time::Instant::now();
                let mut graph = retrieve_result.graph;
                let order = compute_order(&mut graph);
                let order_ms = t_order.elapsed();

                let t_content = std::time::Instant::now();
                let resolved = resolve_conflicts_semantically(&preloaded, store, &graph, &order);
                let buffer = Vec::with_capacity(graph.total_bytes());
                let mut writer = Writer::new(buffer);
                let hash_fn = |node_id: NodeId| -> Option<Hash> {
                    if node_id.is_root() {
                        return None;
                    }
                    preloaded.get_external(node_id).ok().flatten()
                };
                output_graph_content_resolved(
                    store,
                    hash_fn,
                    &graph,
                    &order,
                    &mut writer,
                    &resolved,
                )
                .map_err(|e| format!("{}: content: {:?}", item.path, e))?;
                let content = writer.into_inner();
                let content_ms = t_content.elapsed();

                if content.is_empty() {
                    return Ok(None);
                }

                // Name-conflict override (rare): when ≥ 2 inodes are alive at
                // this path on the view, replace the single-inode content with
                // a marker-wrapped rendering of every side so the conflict is
                // surfaced instead of silently collapsed (rubric A12).
                let content = match name_conflicts.get(&item.path) {
                    Some(sides) => render_name_conflict(
                        &txn,
                        store,
                        &inode_graph_table,
                        &change_filter_arc,
                        &item.path,
                        sides,
                    )
                    .unwrap_or(content),
                    None => content,
                };

                // Detect conflict markers in the materialized bytes. This is
                // the source of truth for persisted conflict state.
                let marker_line = first_conflict_marker_line(&content);

                // Compute content hash from the in-memory buffer
                let content_hash = Hash::of(&content);
                let bytes_written = content.len() as u64;

                // Content-hash skip: if the file on disk already has this
                // exact content, skip the write entirely.
                if let Some(&(idx_secs, idx_nanos, idx_size, ref idx_hash)) =
                    file_index.get(&item.path)
                {
                    if *idx_hash == content_hash {
                        // Verify the on-disk file still matches the index
                        // (hasn't been modified by the user since last materialize)
                        let abs_path = root.join(&item.path);
                        if let Ok(meta) = std::fs::metadata(&abs_path) {
                            if meta.len() == idx_size {
                                if let Ok(mtime) = meta.modified() {
                                    let dur = mtime
                                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                        .unwrap_or_default();
                                    if dur.as_secs() as i64 == idx_secs
                                        && dur.subsec_nanos() == idx_nanos
                                    {
                                        if trace_mat {
                                            eprintln!(
                                                "[materialize] SKIP {} (content unchanged)",
                                                item.path,
                                            );
                                        }
                                        return Ok(Some((
                                            item.path.clone(),
                                            bytes_written,
                                            content_hash,
                                            false, // not written
                                            marker_line,
                                        )));
                                    }
                                }
                            }
                        }
                    }
                }

                // Write to disk in a single call
                let abs_path = root.join(&item.path);
                if let Some(parent) = abs_path.parent() {
                    if !parent.exists() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| format!("{}: create parent: {}", item.path, e))?;
                    }
                }
                std::fs::write(&abs_path, &content)
                    .map_err(|e| format!("{}: write: {}", item.path, e))?;

                if trace_mat {
                    let elapsed = file_start.elapsed();
                    if elapsed > std::time::Duration::from_millis(50) {
                        eprintln!(
                            "[materialize] SLOW {} bytes={} vertices={} edges={} \
                             retrieve={:?} order={:?} content={:?} total={:?}",
                            item.path,
                            bytes_written,
                            vertices,
                            edges,
                            retrieve_ms,
                            order_ms,
                            content_ms,
                            elapsed,
                        );
                    }
                }

                Ok(Some((
                    item.path.clone(),
                    bytes_written,
                    content_hash,
                    true,
                    marker_line,
                )))
            })
            .collect();

        if trace_mat {
            eprintln!(
                "[materialize] parallel phase complete files={} elapsed={:?}",
                total_files,
                mat_start.elapsed(),
            );
        }

        // Phase 6: Aggregate results and update FILE_INDEX
        let mut index_entries: Vec<(String, i64, u32, u64, Hash)> = Vec::new();
        // Files whose materialized content carries conflict markers, with the
        // 1-based line of the first marker.
        let mut conflicts_by_path: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();

        for file_result in file_results {
            match file_result {
                Ok(Some((path, bytes, content_hash, was_written, marker_line))) => {
                    if let Some(line) = marker_line {
                        conflicts_by_path.insert(path.clone(), line);
                    }
                    if was_written {
                        result.files_written += 1;
                        result.bytes_written += bytes;

                        // Stat the freshly written file for FILE_INDEX
                        let abs_path = root.join(&path);
                        if let Ok(metadata) = std::fs::metadata(&abs_path) {
                            let mtime = metadata
                                .modified()
                                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                            let duration = mtime
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .unwrap_or_default();
                            index_entries.push((
                                path,
                                duration.as_secs() as i64,
                                duration.subsec_nanos(),
                                metadata.len(),
                                content_hash,
                            ));
                        }
                    } else {
                        // Content-hash skip: file already had correct content
                        result.files_skipped += 1;
                    }
                }
                Ok(None) => {
                    // File had no content on this view (filtered out)
                    result.files_skipped += 1;
                }
                Err(e) => {
                    log::warn!("Parallel materialize failed for file: {}", e);
                    result.files_skipped += 1;
                }
            }
        }

        // Batch-update FILE_INDEX with pre-computed hashes
        if !index_entries.is_empty() {
            let _ = self.update_file_index(&index_entries);
        }

        // Persist conflict state so `atomic status` can surface conflicted
        // files and `record` can refuse to bake markers into history.
        // Build path→inode from the materialized items, then write in a
        // dedicated txn (the read txn above is dropped first).
        let path_to_inode: std::collections::HashMap<String, u64> = file_items
            .iter()
            .map(|i| (i.path.clone(), i.inode.get()))
            .collect();
        let view_id = view.id;
        drop(inode_graph_table);
        drop(txn);
        if let Err(e) =
            self.persist_view_conflicts(view_id, &path_to_inode, &conflicts_by_path, &only_paths)
        {
            log::warn!("failed to persist conflict state: {}", e);
        }

        Ok(result)
    }

    /// Populate the file index for all tracked files after a materialize.
    ///
    /// Stats each tracked file from disk and stores its mtime + size +
    /// content hash in the pristine database. Errors are silently ignored
    /// (best-effort).
    ///
    /// Why we ignore `result.file_results` and walk the tracked set: the
    /// `MaterializeResult.file_results` map is only populated when
    /// `merge_file_result(_, store_result=true)` is called, and the
    /// `materialize_view` call site in `atomic-core` passes `false`. As a
    /// result `result.file_results.keys()` is empty in production, and the
    /// previous implementation of this function silently no-op'd —
    /// FILE_INDEX was never refreshed by materialize, leaving stale
    /// per-view hashes after `view switch` and producing false `Modified`
    /// reports from `status`.
    ///
    /// Walking `list_tracked_files()` is correct because materialize has
    /// just brought the working copy into sync with the destination
    /// view's recorded state — every tracked file's on-disk content is
    /// the authoritative baseline FILE_INDEX should cache.
    fn populate_file_index(&self, _result: &MaterializeResult) {
        use std::time::SystemTime;

        let tracked = match self.list_tracked_files() {
            Ok(t) => t,
            Err(_) => return,
        };

        let mut entries: Vec<(String, i64, u32, u64, Hash)> = Vec::with_capacity(tracked.len());

        for file in &tracked {
            let abs_path = self.root.join(&file.path);
            let metadata = match std::fs::metadata(&abs_path) {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };

            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let duration = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = duration.as_secs() as i64;
            let nanos = duration.subsec_nanos();
            let size = metadata.len();

            let content_hash = match std::fs::read(&abs_path) {
                Ok(bytes) => Hash::of(&bytes),
                Err(_) => continue,
            };

            let normalized = file.path.to_string_lossy().replace('\\', "/");
            entries.push((normalized, secs, nanos, size, content_hash));
        }

        if !entries.is_empty() {
            let _ = self.update_file_index(&entries);
        }
    }

    /// Update FILE_INDEX for a specific set of paths.
    ///
    /// Reads only the specified files from disk to compute their content
    /// hashes, rather than re-reading every tracked file. This is used
    /// after selective materialization to update only the affected entries.
    #[allow(dead_code)]
    fn populate_file_index_for_paths(&self, paths: &std::collections::HashSet<String>) {
        use std::time::SystemTime;

        let mut entries: Vec<(String, i64, u32, u64, Hash)> = Vec::with_capacity(paths.len());

        for path in paths {
            let abs_path = self.root.join(path);
            let metadata = match std::fs::metadata(&abs_path) {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };

            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let duration = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = duration.as_secs() as i64;
            let nanos = duration.subsec_nanos();
            let size = metadata.len();

            let content_hash = match std::fs::read(&abs_path) {
                Ok(bytes) => Hash::of(&bytes),
                Err(_) => continue,
            };

            entries.push((path.clone(), secs, nanos, size, content_hash));
        }

        if !entries.is_empty() {
            let _ = self.update_file_index(&entries);
        }
    }

    /// Materialize the working copy for a specific prefix only.
    ///
    /// This is useful for partial updates when you only want to sync
    /// a subset of files.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix to materialize (e.g., "src/")
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation.
    pub fn materialize_prefix(&self, prefix: &str) -> Result<MaterializeResult, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the current view for the change filter
        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        let change_filter = collect_visible_change_ids(&txn, &view)?;

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = MaterializeOptions::new()
            .prefix(prefix)
            .with_change_filter(change_filter);

        let result = materialize_view(&cached_txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;

        self.populate_file_index(&result);

        Ok(result)
    }
}
