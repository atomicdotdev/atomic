use std::collections::HashMap;

use super::*;

#[derive(Debug, Clone, Copy)]
enum UntrackedScanPolicy {
    Always,
    Never,
    WhenRegularFileDeleted,
}

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
    pub fn status(
        &self,
        working_copy: WorkingCopyId,
        options: StatusOptions,
    ) -> Result<RepositoryStatus, RepositoryError> {
        self.status_inner(
            working_copy,
            options,
            UntrackedScanPolicy::Always,
            true,
            false,
            None,
        )
    }

    /// Compute status for recording without scanning unrelated untracked files.
    ///
    /// Raw rename detection is the only record stage that consumes untracked
    /// entries. When it is enabled, defer the walk until the selected tracked
    /// status contains a deleted regular file. Untracked content hashes are
    /// also unnecessary because rename matching reads candidates on demand.
    pub(crate) fn status_for_record(
        &self,
        working_copy: WorkingCopyId,
        options: StatusOptions,
        detect_raw_renames: bool,
        include_all_untracked: bool,
    ) -> Result<RepositoryStatus, RepositoryError> {
        let policy = if include_all_untracked {
            UntrackedScanPolicy::Always
        } else if detect_raw_renames {
            UntrackedScanPolicy::WhenRegularFileDeleted
        } else {
            UntrackedScanPolicy::Never
        };
        self.status_inner(working_copy, options, policy, false, true, None)
    }

    fn status_inner(
        &self,
        working_copy: WorkingCopyId,
        options: StatusOptions,
        untracked_policy: UntrackedScanPolicy,
        hash_untracked: bool,
        for_record: bool,
        change_source_override: Option<&dyn crate::change_source::ChangeSource>,
    ) -> Result<RepositoryStatus, RepositoryError> {
        let view_name = self.desired_view_name(working_copy)?;
        let overall_start = std::time::Instant::now();

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;
        let visibility = graph_visibility_closure(&txn, &view)?;
        let content_filter = crate::content_filter::GitAttributesFilter::for_repository(&self.root);

        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &claim_visibility)?;
        let projected_present = projection.present;
        let projected_present_paths: HashSet<PathBuf> =
            projected_present.keys().map(PathBuf::from).collect();
        let projected_absent: Vec<_> = projection.absent_metadata.into_values().collect();
        let projected_name_conflicts = projection.name_conflicts;
        let persisted_name_paths: HashSet<String> = txn
            .iter_conflicts(view.id)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .flat_map(|(_, records)| records)
            .filter(|record| record.kind == atomic_core::pristine::StoredConflictKind::Name)
            .map(|record| record.path)
            .collect();

        let mut status = RepositoryStatus::new(view_name, Some(view.state));

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
        // Files tracked globally (in TREE) but whose introducing change
        // belongs to another view.  These must stay in `tracked_paths` so
        // they don't surface as "Untracked" in the filesystem walk, but if
        // they are absent from disk they must be silently skipped (not
        // reported as "Deleted" — they were never on this view).
        let mut foreign_paths: HashSet<PathBuf> = HashSet::new();

        let tree_iter = txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in tree_iter {
            let (path, inode) = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            // Normalize path once (before view filter so foreign_paths
            // uses the same key as tracked_paths).
            let normalized = normalize_tracked_path(&path, &self.root);

            // View filter: decide whether this file's graph content is
            // visible on the current view.
            //
            // Files in TREE without a graph position → Added (not yet
            // recorded).  Files whose creating change IS in the current
            // view's filter → tracked normally.  Files whose creating
            // change is NOT in the filter → "foreign": they belong to
            // another view.  We keep them in tracked_paths (so the
            // filesystem walk doesn't mark them Untracked) but flag them
            // so that if they're absent from disk we skip them silently
            // instead of reporting Deleted.
            let has_graph = match txn
                .inode_position(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                Some(position)
                    if !position.change.is_root() && !visibility.contains(position.change) =>
                {
                    // Foreign file — tracked globally, not on this view.
                    foreign_paths.insert(normalized.clone());
                    false
                }
                Some(_) => true,
                None => false,
            };

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

        // TREE is aligned to the current visible lifecycle and therefore omits
        // recorded deletions. Reintroduce those paths for status classification
        // using the operation-aware projection so a filesystem reappearance is
        // an undelete of the original inode rather than an untracked new file.
        for absent in &projected_absent {
            let normalized = PathBuf::from(&absent.path);
            // A projected deletion is already the recorded state. Reintroduce
            // it only when an entry actually reappears on disk, where it must
            // be classified as an undelete of the stable inode rather than as
            // an untracked add.
            if std::fs::symlink_metadata(self.root.join(&normalized)).is_err() {
                continue;
            }
            if !options.path_filters.is_empty() {
                let matches = options
                    .path_filters
                    .iter()
                    .any(|f| normalized.starts_with(f) || f.starts_with(&normalized));
                if !matches {
                    continue;
                }
            }
            tracked_paths.insert(normalized.clone());
            inode_map.insert(normalized.clone(), absent.inode);
            has_graph_content_cache.insert(normalized.clone(), true);
            if absent.directory {
                directory_inodes.insert(absent.inode);
            }
        }

        let tree_ms = tree_start.elapsed().as_millis();
        log::debug!(
            "status: TREE scan took {}ms ({} tracked files, {} dirs)",
            tree_ms,
            tracked_paths.len(),
            directory_inodes.len()
        );

        for path in projected_name_conflicts.keys() {
            let normalized = PathBuf::from(path);
            if options.path_filters.is_empty()
                || options
                    .path_filters
                    .iter()
                    .any(|filter| normalized.starts_with(filter) || filter.starts_with(&normalized))
            {
                tracked_paths.insert(normalized);
            }
        }

        // ── Canonical FILE_INDEX_V2 scan and re-verification ────────────
        use crate::change_source::{
            conversion_policy_fingerprint, now_timestamp, verify_candidates, CanonicalTrackedPath,
            ChangeSource, ChangeSourceError, ChangeSourceFallbackReason, ChangeSourceKind,
            ChangeSourceRequest, ChangeSourceTokenStore, ScanChangeSource, SelectedChangeSource,
            VerifiedChange,
        };
        use atomic_core::pristine::FileIndexV2TxnT;

        let index_start = std::time::Instant::now();
        let mut canonical_tracked = Vec::new();
        let mut gitlink_paths: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        for path in &tracked_paths {
            if inode_map
                .get(path)
                .is_some_and(|inode| directory_inodes.contains(inode))
                || !has_graph_content_cache.get(path).copied().unwrap_or(false)
                || !projected_present_paths.contains(path)
                || foreign_paths.contains(path)
            {
                continue;
            }
            let Some(inode) = inode_map.get(path).copied() else {
                continue;
            };
            let Some(position) = txn
                .inode_position(inode)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
            else {
                continue;
            };
            let projected = atomic_core::output::project_inode_attributes(
                &txn,
                position,
                visibility.attribute_visibility(),
            )
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
            if projected.is_conflicted() {
                continue;
            }
            // CB-9C: gitlink paths have no materialized bytes to verify —
            // their content identity is the Git index's object ID, not the
            // submodule directory's disk state — so they are excluded from
            // the canonical content-verification set and recorded for the
            // tracked-file classification loop instead.
            if projected.materialization.kind == atomic_core::change::InodeKind::Gitlink {
                gitlink_paths.insert(path.clone());
                continue;
            }
            let repo_path = crate::repository::RepoPath::from_native(path)
                .map_err(|error| RepositoryError::Output(error.to_string()))?;
            canonical_tracked.push(CanonicalTrackedPath {
                path: repo_path,
                canonical_mode: projected.materialization.mode,
                canonical_kind: projected.materialization.kind,
            });
        }
        canonical_tracked.sort_by(|left, right| left.path.cmp(&right.path));
        let tracked_repo_paths: Vec<_> = canonical_tracked
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        let conversion_policy = conversion_policy_fingerprint(&self.root, &tracked_repo_paths)
            .map_err(map_change_source_error)?;
        let source_request = ChangeSourceRequest {
            root: &self.root,
            tracked_paths: &tracked_repo_paths,
            previous_token: None,
        };
        let token_store = ChangeSourceTokenStore::new(&self.dot_dir, working_copy);
        let mut source_result = if let Some(source) = change_source_override {
            source.changes(source_request)
        } else {
            let git_watch = atomic_config::RepoConfig::load(&self.config_path())
                .map_err(|error| RepositoryError::Config(error.to_string()))?
                .git
                .watch;
            let fsmonitor_token = token_store
                .load(ChangeSourceKind::Fsmonitor)
                .map_err(|error| RepositoryError::Output(error.to_string()))?;
            let watchman_token = token_store
                .load(ChangeSourceKind::Watchman)
                .map_err(|error| RepositoryError::Output(error.to_string()))?;
            if fsmonitor_token.invalidation.is_some() || watchman_token.invalidation.is_some() {
                let mut result = ScanChangeSource
                    .changes(source_request)
                    .map_err(map_change_source_error)?;
                result.fallback_source = Some(if fsmonitor_token.invalidation.is_some() {
                    ChangeSourceKind::Fsmonitor
                } else {
                    ChangeSourceKind::Watchman
                });
                result.fallback = Some(ChangeSourceFallbackReason::UnknownToken);
                Ok(result)
            } else {
                SelectedChangeSource::new(git_watch).changes_with_tokens(
                    source_request,
                    fsmonitor_token.token.as_ref(),
                    watchman_token.token.as_ref(),
                )
            }
        }
        .map_err(map_change_source_error)?;
        let file_index = match txn.iter_file_index_v2(working_copy) {
            Ok(entries) => entries
                .into_iter()
                .map(|(path, entry)| {
                    crate::repository::RepoPath::new(path)
                        .map(|path| (path, entry))
                        .map_err(|error| RepositoryError::Output(error.to_string()))
                })
                .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?,
            Err(error) => {
                source_result.fallback = Some(ChangeSourceFallbackReason::MalformedIndex(
                    error.to_string(),
                ));
                std::collections::BTreeMap::new()
            }
        };
        let index_write_time = now_timestamp();
        let verified_scan = verify_candidates(
            source_result,
            &canonical_tracked,
            &file_index,
            conversion_policy,
            index_write_time,
            &content_filter,
            |path| {
                let path_text = std::str::from_utf8(path.as_bytes()).map_err(|error| {
                    ChangeSourceError::CanonicalContent {
                        path: path.escaped(),
                        message: error.to_string(),
                    }
                })?;
                let item = projected_present.get(path_text).ok_or_else(|| {
                    ChangeSourceError::CanonicalContent {
                        path: path.escaped(),
                        message: "path is absent from the canonical graph projection".into(),
                    }
                })?;
                super::content::retrieve_content_with_filter_fast(
                    &txn,
                    &self.change_store,
                    item.inode,
                    item.position,
                    atomic_core::output::alive::RetrieveOptions::new()
                        .with_graph_visibility(visibility.clone()),
                )
                .map(|bytes| Hash::of(&bytes))
                .map_err(|error| ChangeSourceError::CanonicalContent {
                    path: path.escaped(),
                    message: error.to_string(),
                })
            },
        )
        .map_err(map_change_source_error)?;
        status.set_change_source_verification(verified_scan.root, verified_scan.metrics.clone());
        // CB-13C observability (review R1): status is an observational
        // boundary and never writes telemetry — including from read-only
        // opens, non-Git repositories or un-opted configurations. The
        // degradation metrics are returned to the caller here (see
        // `change_source_metrics`); only explicit bridge command boundaries
        // decide whether an authorized journal records them.
        let verified_by_path: HashMap<PathBuf, _> = verified_scan
            .candidates
            .iter()
            .filter_map(|candidate| {
                candidate
                    .path
                    .to_native()
                    .ok()
                    .map(|path| (path, candidate.clone()))
            })
            .collect();
        let source_token = verified_scan.token.clone();
        let source_metrics = verified_scan.metrics.clone();
        let v2_updates = verified_scan.index_updates;
        let v2_deletions = verified_scan.index_deletions;
        let index_ms = index_start.elapsed().as_millis();
        log::debug!(
            "status: FILE_INDEX_V2 verified {}ms (tracked={}, leases={}, hashed={}, fallback={:?}, root={:?})",
            index_ms,
            verified_scan.stats.tracked_paths,
            verified_scan.stats.cache_leases,
            verified_scan.stats.content_reads,
            verified_scan.fallback,
            verified_scan.root
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

            // CB-9C: a tracked gitlink materializes as a submodule
            // directory. Its content identity lives in the Git index, its
            // disk permissions are foreign state, and its submodule
            // contents are not Atomic untracked content — so the gitlink
            // path itself is present and clean unless the projected state
            // says otherwise.
            if gitlink_paths.contains(path) {
                found_on_disk.insert(path.clone());
                if !abs_path.is_dir() {
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::TypeChanged);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    status.add_entry(entry);
                }
                continue;
            }

            let is_dir = inode
                .map(|i| directory_inodes.contains(&i))
                .unwrap_or(false);

            // Skip tracked directories — handle separately
            if is_dir {
                found_on_disk.insert(path.clone());
                let projected_present = projected_present_paths.contains(path);
                if abs_path.is_dir() {
                    if !has_graph || !projected_present {
                        // A projected-absent directory that reappears is an
                        // undelete candidate carrying its original inode.
                        let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Added);
                        if let Some(inode) = inode {
                            entry.set_inode(inode);
                        }
                        entry.set_details("directory".to_string());
                        status.add_entry(entry);
                    }
                } else if has_graph && !projected_present {
                    // The deletion is already recorded on this view.
                } else {
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Deleted);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    entry.set_details("directory".to_string());
                    status.add_entry(entry);
                }
                continue;
            }

            // Check if file exists on disk without following symlinks. Dangling
            // links remain present and their target bytes are versioned content.
            stat_count += 1;
            let _metadata = match std::fs::symlink_metadata(&abs_path) {
                Ok(m) if m.is_file() || m.file_type().is_symlink() || (m.is_dir() && !is_dir) => m,
                _ => {
                    // Foreign file not on disk — skip silently.
                    // This file is tracked globally (in TREE) but its
                    // graph content belongs to another view and it does
                    // not exist on disk for THIS view.  Reporting it as
                    // "Deleted" would be wrong (it was never present on
                    // this view).
                    if foreign_paths.contains(path) {
                        found_on_disk.insert(path.clone());
                        continue;
                    }

                    // Lifecycle projection, not content length, decides whether
                    // the missing path is an already-recorded deletion. A
                    // present zero-byte file remains in this set and is reported
                    // Deleted when missing from disk.
                    if has_graph && !projected_present_paths.contains(path) {
                        found_on_disk.insert(path.clone());
                        continue;
                    }
                    // File is genuinely missing and deletion not yet recorded
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

            if has_graph {
                if let (Some(inode), Some(position)) = (
                    inode,
                    inode.and_then(|inode| txn.inode_position(inode).ok().flatten()),
                ) {
                    let projected = atomic_core::output::project_inode_attributes(
                        &txn,
                        position,
                        visibility.attribute_visibility(),
                    )
                    .map_err(|error| RepositoryError::Database(error.to_string()))?;
                    if projected.is_conflicted() {
                        let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Conflicted);
                        entry.set_inode(inode);
                        entry.set_details("inode attribute conflict".to_string());
                        status.add_or_replace_entry(entry);
                        continue;
                    }
                    let actual = super::attributes::working_inode_attrs(&abs_path)?;
                    let facts = atomic_core::output::InodeStatusFacts::between(
                        projected.materialization,
                        actual.mode,
                        actual.kind,
                    );
                    // CB-9C: a gitlink's registered mode is not a
                    // materialized fact — the submodule directory's own
                    // permission bits are foreign state, so a mode
                    // difference on a Gitlink kind is never a reportable
                    // permission change. The materialized directory mode is
                    // whatever the filesystem created.
                    let permissions_changed = facts.permissions_changed
                        && projected.materialization.kind
                            != atomic_core::change::InodeKind::Gitlink;
                    if facts.type_changed {
                        let mut entry = FileStatusEntry::new(path.clone(), FileStatus::TypeChanged);
                        entry.set_inode(inode);
                        status.add_entry(entry);
                        continue;
                    }
                    if permissions_changed {
                        let mut entry =
                            FileStatusEntry::new(path.clone(), FileStatus::PermissionsChanged);
                        entry.set_inode(inode);
                        status.add_entry(entry);
                        continue;
                    }
                }
            }

            if has_graph && !projected_present_paths.contains(path) {
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

            // The source result is only a candidate set. Canonical V2
            // re-verification has already checked metadata, racy-stat, policy,
            // graph mode/kind, and repository bytes before a clean omission.
            if let Some(verified) = verified_by_path.get(path) {
                match verified.change {
                    VerifiedChange::Unchanged => {
                        if verified.rehashed {
                            hash_count += 1;
                        } else {
                            index_hit_count += 1;
                        }
                        continue;
                    }
                    VerifiedChange::Modified => {
                        if verified.rehashed {
                            hash_count += 1;
                        }
                        let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
                        if let Some(inode) = inode {
                            entry.set_inode(inode);
                        }
                        if let Some(current_hash) = verified.content_id {
                            entry.set_current_hash(current_hash);
                        }
                        status.add_entry(entry);
                        continue;
                    }
                    VerifiedChange::Deleted
                    | VerifiedChange::TypeChanged
                    | VerifiedChange::PermissionsChanged => {
                        // Deletion and canonical attribute changes were already
                        // classified above with the existing status semantics.
                    }
                }
            }

            // Paths excluded from the canonical scanner (for example an
            // unresolved attribute conflict) retain the conservative behavior.
            status.add_stale_index_hit();
            let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
            if let Some(inode) = inode {
                entry.set_inode(inode);
            }
            entry.set_details("FILE_INDEX_V2 verification unavailable".to_string());
            status.add_entry(entry);
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
        let scan_untracked = options.include_untracked
            && match untracked_policy {
                UntrackedScanPolicy::Always => true,
                UntrackedScanPolicy::Never => false,
                UntrackedScanPolicy::WhenRegularFileDeleted => status
                    .entries()
                    .iter()
                    .any(|e| e.status() == FileStatus::Deleted && e.details() != Some("directory")),
            };
        log::debug!(
            "status: untracked policy={:?}, scan={}",
            untracked_policy,
            scan_untracked
        );
        if scan_untracked {
            let rules = if options.respect_ignore_files {
                Some(self.load_ignore_rules())
            } else {
                None
            };

            // Review CB-9C R5: prune only authoritative nested-repository
            // boundaries. Tracked visible gitlink paths are passed
            // explicitly; every other directory is pruned only when its
            // `.git` marker is an actual repository, so incidental `.git`
            // markers never hide ordinary parent content.
            let walker_options = options.clone().with_nested_repo_boundaries(
                gitlink_paths.iter().cloned(),
            );
            let working_files =
                collect_working_copy_files_with_rules(&self.root, &walker_options, rules.as_ref())
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;

            for path in working_files {
                if !tracked_paths.contains(&path) {
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Untracked);
                    if hash_untracked && options.hash_contents {
                        let abs_path = self.root.join(&path);
                        if let Ok(hash) = hash_file_contents(&abs_path) {
                            entry.set_current_hash(hash);
                        }
                    }
                    status.add_entry(entry);
                }
            }
        }

        // PATH_CLAIMS conflicts are authoritative and visible even before a
        // materialize has persisted compatibility conflict rows. A12 file
        // conflicts become recordable Modified entries once markers are removed;
        // A11 and directory conflicts have no in-file marker channel.
        for (path, conflict) in &projected_name_conflicts {
            let normalized = PathBuf::from(path);
            if !options.path_filters.is_empty()
                && !options
                    .path_filters
                    .iter()
                    .any(|filter| normalized.starts_with(filter) || filter.starts_with(&normalized))
            {
                continue;
            }
            let abs_path = self.root.join(&normalized);
            let has_markers = std::fs::read(&abs_path)
                .ok()
                .and_then(|bytes| super::materialize::first_conflict_marker_line(&bytes))
                .is_some();
            let path_sides = conflict.sides_at_path(path);
            let marker_resolved_a12 = for_record
                && persisted_name_paths.contains(path)
                && !conflict.is_rename_conflict()
                && !has_markers
                && path_sides.iter().all(|side| !side.is_directory())
                && abs_path.is_file();
            let mut entry = FileStatusEntry::new(
                normalized,
                if marker_resolved_a12 {
                    FileStatus::Modified
                } else {
                    FileStatus::Conflicted
                },
            );
            if path_sides.len() == 1 {
                entry.set_inode(path_sides[0].inode);
            }
            entry.set_details(format!(
                "name conflict ({} path(s), {} claimant(s))",
                conflict.paths.len(),
                conflict.sides.len()
            ));
            status.add_or_replace_entry(entry);
        }

        // ── Conflicted files ────────────────────────────────────────────
        //
        // Surface persisted conflict state (written by the last materialize
        // on this view) so a conflicted working tree is never reported clean.
        // A Conflicted entry supersedes any Modified entry for the same path.
        let conflicts = txn
            .iter_conflicts(view.id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for (inode, records) in conflicts {
            let Some(first) = records.first() else {
                continue;
            };
            let path = PathBuf::from(&first.path);
            // Honesty invariant: only report Conflicted while the file on
            // disk still carries markers. Once the user resolves them the
            // file falls back to normal Modified detection and becomes
            // recordable again (which then clears the stale entry).
            let abs_path = self.root.join(&path);
            let still_conflicted = std::fs::read(&abs_path)
                .ok()
                .and_then(|c| super::materialize::first_conflict_marker_line(&c))
                .is_some();
            if !still_conflicted {
                continue;
            }
            let detail = if records.len() > 1 {
                format!("{} ({} conflicts)", first.summary(), records.len())
            } else {
                first.summary()
            };
            let mut entry = FileStatusEntry::new(path, FileStatus::Conflicted);
            entry.set_inode(atomic_core::types::Inode::new(inode));
            entry.set_details(detail);
            // Conflicted supersedes any prior (e.g. Modified) entry so the
            // file is reported exactly once.
            status.add_or_replace_entry(entry);
        }

        // RFC §8.3 (CB-8B) — the essential conflict caveat: when this view's
        // conflict state has been projected as an ordinary Git conflict
        // snapshot commit (clean Git index, committed markers), the notice
        // must make the distinction explicit. Committed markers are a lossy
        // Git representation, NOT Git unmerged stages 1-3: Git status is
        // clean for them and merge tools do not apply, while the Atomic
        // conflict remains unresolved until it is resolved in Atomic.
        if status.entries().iter().any(|entry| entry.status() == FileStatus::Conflicted) {
            let git_snapshot = git2::Repository::open(&self.root)
                .ok()
                .and_then(|git| {
                    let head = git.head().ok()?.target()?;
                    let commit = git.find_commit(head).ok()?;
                    let message = commit.message().unwrap_or("");
                    Some(
                        message
                            .lines()
                            .any(|line| line.trim().starts_with("atomic-conflict ")),
                    )
                })
                .unwrap_or(false);
            if git_snapshot {
                status.add_notice(
                    "Git HEAD is an Atomic conflict snapshot commit: marker files are \
                     committed on a CLEAN Git index (not Git unmerged stages 1-3). The \
                     Atomic conflict remains unresolved; resolve it in Atomic and record \
                     before treating the state as clean."
                        .to_string(),
                );
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

        drop(txn);
        if let Err(error) =
            self.update_file_index_v2_transaction(working_copy, &v2_updates, &v2_deletions)
        {
            log::warn!("status: unable to persist FILE_INDEX_V2 transaction: {error}");
        }
        if source_metrics.fallback_reason == Some(ChangeSourceFallbackReason::UnknownToken) {
            if let Some(source) = source_metrics.fallback_source {
                if let Err(error) = token_store.invalidate(source) {
                    log::warn!("status: unable to invalidate change-source token: {error}");
                }
            }
        } else if source_metrics.fallback_count == 0
            && matches!(
                source_metrics.source,
                ChangeSourceKind::Fsmonitor | ChangeSourceKind::Watchman
            )
        {
            if let Err(error) = token_store.store(source_metrics.source, &source_token) {
                log::warn!("status: unable to persist change-source token: {error}");
            }
        }

        Ok(status)
    }

    #[cfg(test)]
    fn status_with_change_source(
        &self,
        working_copy: WorkingCopyId,
        options: StatusOptions,
        source: &dyn crate::change_source::ChangeSource,
    ) -> Result<RepositoryStatus, RepositoryError> {
        self.status_inner(
            working_copy,
            options,
            UntrackedScanPolicy::Never,
            false,
            false,
            Some(source),
        )
    }

    /// List the current view's persisted conflicts.
    ///
    /// Returns `(path, conflicts)` pairs for every conflicted file, sorted by
    /// path. Honesty invariant: a file is included only while its on-disk
    /// content still carries conflict markers (matching
    /// [`Repository::status`]); a resolved-but-not-yet-recorded file is
    /// omitted. Read-only.
    #[allow(clippy::type_complexity)]
    pub fn list_conflicts(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<(String, Vec<atomic_core::pristine::StoredConflict>)>, RepositoryError> {
        let view_name = self.desired_view_name(working_copy)?;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = match txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };
        let full_visibility = graph_visibility_closure(&txn, &view)?;
        let visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &full_visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        let active_name_paths: HashSet<String> = projection
            .name_conflicts
            .iter()
            .filter_map(|(path, conflict)| {
                let sides = conflict.sides_at_path(path);
                let requires_marker =
                    !conflict.is_rename_conflict() && sides.iter().all(|side| !side.is_directory());
                let has_marker = std::fs::read(self.root.join(path))
                    .ok()
                    .and_then(|bytes| super::materialize::first_conflict_marker_line(&bytes))
                    .is_some();
                (!requires_marker || has_marker).then(|| path.clone())
            })
            .collect();
        let mut by_path = HashMap::<String, Vec<atomic_core::pristine::StoredConflict>>::new();
        for (_inode, records) in txn
            .iter_conflicts(view.id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            for record in records {
                let still_conflicted = if record.kind
                    == atomic_core::pristine::StoredConflictKind::Name
                {
                    active_name_paths.contains(&record.path)
                } else {
                    std::fs::read(self.root.join(&record.path))
                        .ok()
                        .and_then(|bytes| super::materialize::first_conflict_marker_line(&bytes))
                        .is_some()
                };
                if still_conflicted {
                    by_path.entry(record.path.clone()).or_default().push(record);
                }
            }
        }
        for (path, conflict) in projection.name_conflicts {
            if !active_name_paths.contains(&path) {
                continue;
            }
            by_path.entry(path.clone()).or_insert_with(|| {
                vec![atomic_core::pristine::StoredConflict {
                    kind: atomic_core::pristine::StoredConflictKind::Name,
                    path,
                    line: None,
                    sides: conflict
                        .sides
                        .iter()
                        .flat_map(|side| side.event_changes.iter())
                        .map(|change| change.get().to_string())
                        .collect(),
                }]
            });
        }
        let mut out: Vec<_> = by_path.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Quick status check — uses default options.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status_quick(working_copy)?;
    /// println!("Modified: {}", status.modified_count());
    /// ```
    pub fn status_quick(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<RepositoryStatus, RepositoryError> {
        self.status(working_copy, StatusOptions::fast())
    }

    /// Status showing only tracked files (no untracked).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status_tracked(working_copy)?;
    /// // Only shows modified, deleted, added - no untracked
    /// ```
    pub fn status_tracked(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<RepositoryStatus, RepositoryError> {
        self.status(working_copy, StatusOptions::tracked_only())
    }

    /// Check if the working copy is clean (no modifications).
    pub fn is_working_copy_clean(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<bool, RepositoryError> {
        let status = self.status(working_copy, StatusOptions::fast())?;
        Ok(status.is_clean())
    }

    /// Get only modified files.
    pub fn modified_files(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(working_copy, StatusOptions::default())?;
        Ok(status.modified().map(|e| e.path().to_path_buf()).collect())
    }

    /// Get only untracked files.
    pub fn untracked_files(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(working_copy, StatusOptions::default())?;
        Ok(status.untracked().map(|e| e.path().to_path_buf()).collect())
    }

    /// Get only deleted files.
    pub fn deleted_files(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(working_copy, StatusOptions::default())?;
        Ok(status.deleted().map(|e| e.path().to_path_buf()).collect())
    }
}

/// Check whether an explicit delete still has surviving content spans.
///
/// This is only a tie-breaker for lifecycle projection: an ordinary tracked
/// empty file is present because its path lifecycle says so. For a visible
/// `FileDel`, surviving spans from concurrent modifications keep the file
/// present; a graph with no live byte spans leaves the deletion absent.
pub(crate) fn is_file_alive_via_retrieval<T: GraphTxnT>(
    txn: &T,
    _inode: Inode,
    position: Position<NodeId>,
    visibility: &GraphVisibilityClosure,
) -> Result<bool, RepositoryError> {
    use atomic_core::output::alive::{retrieve_graph, RetrieveOptions};

    // Liveness fast path (review E4): this check runs for EVERY absent
    // path-claim entry on every projection, and a full alive-graph walk per
    // entry is quadratic on long-history repositories (a single file with a
    // deep history took minutes). The question is exactly "does this position
    // render any alive bytes?", so the traversal can stop at the first alive
    // content vertex; the partial graph's byte total is the whole verdict.
    let options = RetrieveOptions::new()
        .with_graph_visibility(visibility.clone())
        .stop_at_first_content(true);
    let retrieved = retrieve_graph(txn, position, options)
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
    Ok(retrieved.graph.total_bytes() > 0)
}

fn map_change_source_error(error: crate::change_source::ChangeSourceError) -> RepositoryError {
    RepositoryError::Output(error.to_string())
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

#[cfg(all(test, unix))]
mod change_source_convergence_tests {
    use std::sync::Arc;

    use atomic_config::GitWatch;
    use atomic_core::change::ChangeHeader;
    use tempfile::tempdir;

    use super::*;
    use crate::change_source::{
        ChangeSource, ChangeSourceEnvironment, ChangeSourceError, ChangeSourceRequest,
        ChangeSourceResult, ScanChangeSource, SelectedChangeSource,
    };
    use crate::record::RecordOptions;

    #[derive(Clone, Copy)]
    enum CandidateMutation {
        Exact,
        Dropped,
        DuplicateReordered,
    }

    struct CandidateSource(CandidateMutation);

    impl ChangeSource for CandidateSource {
        fn changes(
            &self,
            request: ChangeSourceRequest<'_>,
        ) -> Result<ChangeSourceResult, ChangeSourceError> {
            let mut result = ScanChangeSource.changes(request)?;
            result.complete = false;
            match self.0 {
                CandidateMutation::Exact => {}
                CandidateMutation::Dropped => {
                    result.candidates.pop();
                }
                CandidateMutation::DuplicateReordered => {
                    if let Some(candidate) = result.candidates.first().cloned() {
                        result.candidates.push(candidate);
                    }
                    result.candidates.reverse();
                }
            }
            result.stats.candidates = result.candidates.len();
            Ok(result)
        }
    }

    struct FailingSource(ChangeSourceError);

    impl ChangeSource for FailingSource {
        fn changes(
            &self,
            _: ChangeSourceRequest<'_>,
        ) -> Result<ChangeSourceResult, ChangeSourceError> {
            Err(self.0.clone())
        }
    }

    fn status_shape(status: &RepositoryStatus) -> Vec<(PathBuf, FileStatus)> {
        let mut entries: Vec<_> = status
            .entries()
            .iter()
            .map(|entry| (entry.path().to_path_buf(), entry.status()))
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    fn selected(
        mode: GitWatch,
        fsmonitor: Arc<dyn ChangeSource>,
        watchman: Arc<dyn ChangeSource>,
    ) -> SelectedChangeSource {
        SelectedChangeSource::with_sources(
            mode,
            ChangeSourceEnvironment::default(),
            fsmonitor,
            watchman,
        )
    }

    #[test]
    fn scan_adapters_candidate_disorder_and_failures_converge_in_status() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"one").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"two").unwrap();
        repo.add(working_copy, "a.txt", TrackingOptions::default())
            .unwrap();
        repo.add(working_copy, "b.txt", TrackingOptions::default())
            .unwrap();
        repo.record(
            working_copy,
            ChangeHeader::new("seed"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

        repo.status_with_change_source(
            working_copy,
            StatusOptions::tracked_only(),
            &ScanChangeSource,
        )
        .unwrap();
        std::fs::write(dir.path().join("b.txt"), b"changed").unwrap();

        let scan = repo
            .status_with_change_source(
                working_copy,
                StatusOptions::tracked_only(),
                &ScanChangeSource,
            )
            .unwrap();
        let expected_root = scan.verified_candidate_root().unwrap();
        let expected_status = status_shape(&scan);
        let scan_metrics = scan.change_source_metrics().unwrap();
        assert_eq!(
            scan_metrics.source,
            crate::change_source::ChangeSourceKind::Scan
        );
        assert_eq!(scan_metrics.candidate_count, 2);
        assert_eq!(scan_metrics.metadata_reads, 2);
        assert_eq!(scan_metrics.content_reads, 1);
        assert_eq!(scan_metrics.content_hashes, 1);
        assert_eq!(scan_metrics.valid_lease_hits, 1);
        assert_eq!(scan_metrics.fallback_count, 0);

        for source in [
            selected(
                GitWatch::Fsmonitor,
                Arc::new(CandidateSource(CandidateMutation::Exact)),
                Arc::new(CandidateSource(CandidateMutation::Exact)),
            ),
            selected(
                GitWatch::Watchman,
                Arc::new(CandidateSource(CandidateMutation::Exact)),
                Arc::new(CandidateSource(CandidateMutation::Exact)),
            ),
            selected(
                GitWatch::Fsmonitor,
                Arc::new(CandidateSource(CandidateMutation::Dropped)),
                Arc::new(CandidateSource(CandidateMutation::Exact)),
            ),
            selected(
                GitWatch::Watchman,
                Arc::new(CandidateSource(CandidateMutation::Exact)),
                Arc::new(CandidateSource(CandidateMutation::DuplicateReordered)),
            ),
        ] {
            let status = repo
                .status_with_change_source(working_copy, StatusOptions::tracked_only(), &source)
                .unwrap();
            assert_eq!(status.verified_candidate_root(), Some(expected_root));
            assert_eq!(status_shape(&status), expected_status);
        }

        let failures = [
            ChangeSourceError::Unavailable("missing".into()),
            ChangeSourceError::UnsupportedVersion {
                found: "2.1.0".into(),
                minimum: "2.37.0".into(),
            },
            ChangeSourceError::UnknownToken,
            ChangeSourceError::Overflow { limit: 1 },
            ChangeSourceError::MalformedResponse("bad json".into()),
            ChangeSourceError::Timeout { milliseconds: 1 },
        ];
        for failure in failures {
            let source = selected(
                GitWatch::Fsmonitor,
                Arc::new(FailingSource(failure)),
                Arc::new(CandidateSource(CandidateMutation::Exact)),
            );
            let status = repo
                .status_with_change_source(working_copy, StatusOptions::tracked_only(), &source)
                .unwrap();
            assert_eq!(status.verified_candidate_root(), Some(expected_root));
            assert_eq!(status_shape(&status), expected_status);
            assert!(status.change_source_fallback().is_some());
            let metrics = status.change_source_metrics().unwrap();
            assert_eq!(metrics.fallback_count, 1);
            assert_eq!(
                metrics.fallback_source,
                Some(crate::change_source::ChangeSourceKind::Fsmonitor)
            );
            assert!(metrics.fallback_reason.is_some());
        }
    }

    /// Review R1: status is observational and must not write bridge
    /// telemetry — not from a read-only open, a non-Git repository, or a
    /// configuration with the bridge un-opted. A degrading change source
    /// still surfaces its metrics to the caller, but no event file may be
    /// created (this failed before: the fallback emitted into
    /// `.atomic/bridge/events.jsonl` unconditionally).
    #[test]
    fn status_fallback_writes_no_event_journal() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"one").unwrap();
        repo.add(working_copy, "a.txt", TrackingOptions::default())
            .unwrap();
        repo.record(
            working_copy,
            ChangeHeader::new("seed"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

        let source = selected(
            GitWatch::Fsmonitor,
            Arc::new(FailingSource(ChangeSourceError::Unavailable("missing".into()))),
            Arc::new(CandidateSource(CandidateMutation::Exact)),
        );
        let status = repo
            .status_with_change_source(working_copy, StatusOptions::tracked_only(), &source)
            .unwrap();

        // The degradation metrics are returned to the caller (the
        // explicitly authorized sink boundary decides what to record).
        let metrics = status.change_source_metrics().unwrap();
        assert_eq!(metrics.fallback_count, 1);
        assert!(status.change_source_fallback().is_some());

        let journal_path = dir
            .path()
            .join(crate::repository::DOT_DIR)
            .join("bridge/events.jsonl");
        assert!(
            !journal_path.exists(),
            "observational status must never write bridge telemetry"
        );
    }
}
