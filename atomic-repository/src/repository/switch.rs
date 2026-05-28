use super::*;

/// Return the workspace directory path for a given view.
///
/// The path is `.atomic/workspaces/<view_name>/`.  View names may
/// contain `/` (e.g. `agent/ses_abc123`), which becomes a nested
/// directory structure.
pub(super) fn workspace_path(dot_dir: &Path, view_name: &str) -> PathBuf {
    dot_dir.join(WORKSPACES_DIR).join(view_name)
}

/// Ensure the workspace directory for a view exists.
///
/// Creates `.atomic/workspaces/<view_name>/` and any intermediate
/// directories.  This is called from `init`, `create_view`, and
/// `create_view_from`.
pub(super) fn ensure_workspace_dir(dot_dir: &Path, view_name: &str) -> Result<(), RepositoryError> {
    let ws = workspace_path(dot_dir, view_name);
    std::fs::create_dir_all(&ws)?;
    Ok(())
}

/// Remove empty ancestor directories after file removal.
///
/// Given an iterator of relative paths that were just deleted, this
/// collects every parent directory, sorts them deepest-first, and
/// attempts `std::fs::remove_dir` on each.  Because `remove_dir` only
/// succeeds on *empty* directories, this is always safe — a directory
/// that still contains files (tracked, untracked, or otherwise) will
/// simply fail silently.
///
/// Extracting this into a standalone helper keeps `switch_view` at the
/// orchestration level and makes the cleanup logic reusable for other
/// operations (e.g. `atomic clean`).
fn cleanup_empty_ancestors<'a>(root: &Path, removed_paths: impl Iterator<Item = &'a str>) {
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    for path in removed_paths {
        let p = PathBuf::from(path);
        let mut ancestor = p.parent();
        while let Some(dir) = ancestor {
            if dir == Path::new("") || dir == Path::new(".") {
                break;
            }
            dirs.insert(dir.to_path_buf());
            ancestor = dir.parent();
        }
    }
    // Sort deepest-first so children are removed before parents.
    let mut sorted: Vec<PathBuf> = dirs.into_iter().collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.components().count()));
    for dir in sorted {
        let abs = root.join(&dir);
        if abs.is_dir() {
            // Only succeeds if the directory is empty — safe by construction.
            let _ = std::fs::remove_dir(&abs);
        }
    }
}

impl Repository {
    /// Switch to a different view and update the working copy.
    ///
    /// This is the primary method for switching views. It:
    /// 1. Validates the view exists
    /// 2. Updates the current view pointer
    /// 3. Materializes the working copy to match the new view's state
    ///
    /// # Arguments
    ///
    /// * `view` - The name of the view to switch to
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation (files written, etc.)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view does not exist
    /// - The working copy cannot be updated
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut repo = Repository::open(".")?;
    ///
    /// // Switch to feature view and update working copy
    /// let result = repo.switch_view("feature")?;
    /// println!("Updated {} files", result.files_written);
    /// ```
    pub fn switch_view(&mut self, view: &str) -> Result<MaterializeResult, RepositoryError> {
        let old_view_name = self.current_view.clone();

        // Compute files visible on the OLD view.
        let old_files = self.visible_file_paths(&old_view_name)?;

        // Set the current view (validates it exists)
        self.set_current_view(view)?;

        // Compute files visible on the NEW view.
        let new_files = self.visible_file_paths(view)?;

        let working_copy = FileSystem::from_root(&self.root);

        // ── Phase 1: Shelve ignored files into the OLD view's workspace ──
        //
        // All ignored files are shelved per-view EXCEPT paths listed in
        // `[workspace] expose` in `.atomic/config.toml`.  Exposed paths
        // persist across all views (tool configs like .opencode/, .vscode/).
        //
        // This uses `rename()` which is O(1) on the same filesystem —
        // no data is copied, just inode pointers are updated.
        //
        // The rule:
        //   - Tracked files      → managed by the graph (phases 2-4)
        //   - Untracked, ignored, exposed  → left alone (persists across views)
        //   - Untracked, ignored, NOT exposed → shelved/restored per-view (phases 1 & 5)
        //   - Untracked, novel   → user's undecided work, left alone
        let old_ws = workspace_path(&self.dot_dir, &old_view_name);
        ensure_workspace_dir(&self.dot_dir, &old_view_name)?;

        let repo_expose = atomic_config::RepoConfig::load(&self.config_path())
            .unwrap_or_default()
            .workspace
            .expose;
        let global_expose = atomic_config::GlobalConfig::load()
            .map(|c| c.workspace.expose)
            .unwrap_or_default();

        // Merge global + repo-local expose patterns (deduplicated)
        let mut expose_patterns = global_expose;
        for p in repo_expose {
            if !expose_patterns.contains(&p) {
                expose_patterns.push(p);
            }
        }

        let ignored_paths: Vec<String> = self
            .collect_ignored_paths_on_disk()
            .into_iter()
            .filter(|path| {
                // Keep paths that are NOT exposed (those get shelved)
                !expose_patterns
                    .iter()
                    .any(|pattern| path == pattern || path.starts_with(&format!("{}/", pattern)))
            })
            .collect();
        if !ignored_paths.is_empty() {
            // Clear old workspace content, then move current ignored files in.
            // We clear first because the workspace may contain stale state
            // from a previous shelve.
            for path in &ignored_paths {
                let ws_dest = old_ws.join(path);
                // Remove stale entry in workspace if it exists
                if ws_dest.is_dir() {
                    let _ = std::fs::remove_dir_all(&ws_dest);
                } else if ws_dest.exists() {
                    let _ = std::fs::remove_file(&ws_dest);
                }
                // Ensure parent dirs exist in workspace
                if let Some(parent) = ws_dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                // Move from working copy → workspace (O(1) rename)
                let src = self.root.join(path);
                if src.exists() {
                    let _ = std::fs::rename(&src, &ws_dest);
                }
            }
        }

        // ── Phase 2: Remove tracked files that belong to the old view ──
        //
        // Files visible on the old view but NOT on the new view are
        // removed from disk.
        let mut removed_paths: Vec<String> = Vec::new();
        for path in old_files.difference(&new_files) {
            let abs_path = self.root.join(path);
            if abs_path.exists()
                && !abs_path.is_dir()
                && working_copy.remove_path(path, false).is_ok()
            {
                removed_paths.push(path.clone());
            }
        }

        // ── Phase 3: Clean up empty ancestor directories ────────────────
        let all_removed = removed_paths
            .iter()
            .map(|s| s.as_str())
            .chain(ignored_paths.iter().map(|s| s.as_str()));
        cleanup_empty_ancestors(&self.root, all_removed);

        // ── Phase 4: Materialize the new view's tracked files from graph ─
        //
        // Instead of materializing ALL files, compute which files differ
        // between the old and new views and only materialize those.
        // Files shared between views with identical content are untouched.
        let result = {
            let mut affected_paths: HashSet<String> = HashSet::new();

            // Files only on the new view must always be materialized.
            for path in new_files.difference(&old_files) {
                affected_paths.insert(path.clone());
            }

            // Find changes that differ between the two views.
            // Use the FULL visible change sets (including parent chains)
            // so that inherited changes don't show up as differences.
            // get_view_changes only returns a view's OWN change log,
            // which would flag every inherited change as "different".
            {
                let txn = self
                    .pristine
                    .read_txn()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;

                let old_view = txn
                    .get_view(&old_view_name)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: old_view_name.clone(),
                    })?;
                let new_view = txn
                    .get_view(view)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: view.to_string(),
                    })?;

                let old_ids = collect_visible_change_ids(&txn, &old_view)?;
                let new_ids = collect_visible_change_ids(&txn, &new_view)?;

                // NodeIds that are in one view but not the other
                let diff_ids: Vec<_> = old_ids.symmetric_difference(&new_ids).copied().collect();

                for node_id in &diff_ids {
                    if let Ok(Some(hash)) = txn.get_external(*node_id) {
                        if let Ok(change) = self.load_change(&hash) {
                            for op in change.hunks() {
                                if let Some(p) = op.path() {
                                    affected_paths.insert(p.to_string());
                                }
                            }
                        }
                    }
                }
            }

            if affected_paths.is_empty() {
                self.materialize()?
            } else {
                self.materialize_paths(affected_paths)?
            }
        };

        // ── Phase 5: Restore ignored files from the NEW view's workspace ─
        //
        // Move artifacts from `.atomic/workspaces/<new_view>/` back into
        // the working copy.  Again O(1) renames, no data copying.
        let new_ws = workspace_path(&self.dot_dir, view);
        if new_ws.is_dir() {
            self.restore_workspace_to_working_copy(&new_ws);
        }

        Ok(result)
    }

    /// Restore entries from a workspace directory into the working copy.
    ///
    /// Walks the top-level entries in `ws_dir` and moves each into the
    /// project root via `rename()`.  Skips the `.atomic` directory if
    /// present.
    fn restore_workspace_to_working_copy(&self, ws_dir: &Path) {
        let entries = match std::fs::read_dir(ws_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Never move .atomic into the working copy
            if name_str == DOT_DIR {
                continue;
            }

            let src = entry.path();
            let dst = self.root.join(&*name_str);

            // If the destination already exists (e.g. a directory that
            // was created by materialize for tracked content),
            // merge by recursing into it rather than replacing it.
            if dst.is_dir() && src.is_dir() {
                self.merge_dir_into(&src, &dst);
                let _ = std::fs::remove_dir_all(&src);
            } else {
                // Ensure parent exists
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&src, &dst);
            }
        }
    }

    /// Recursively merge the contents of `src_dir` into `dst_dir`.
    ///
    /// Files in `src_dir` are moved into `dst_dir`.  If a subdirectory
    /// exists in both, the merge recurses.  This is used when restoring
    /// workspace artifacts into a directory that already contains tracked
    /// files (e.g. `src/` might have tracked `.ts` files from the graph
    /// AND ignored `.cache/` from the workspace).
    fn merge_dir_into(&self, src_dir: &Path, dst_dir: &Path) {
        let entries = match std::fs::read_dir(src_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let src = entry.path();
            let dst = dst_dir.join(&name);

            if dst.is_dir() && src.is_dir() {
                self.merge_dir_into(&src, &dst);
                let _ = std::fs::remove_dir_all(&src);
            } else {
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&src, &dst);
            }
        }
    }

    /// Walk the working copy and collect relative paths of files and
    /// directories that match `.atomicignore` rules.
    ///
    /// Only top-level ignored entries are returned — if `node_modules/`
    /// matches, we return `"node_modules"` rather than enumerating every
    /// file inside it (the caller will `remove_dir_all`).
    ///
    /// Paths that live inside `.atomic/` are never returned.
    fn collect_ignored_paths_on_disk(&self) -> Vec<String> {
        let rules = self.ignore_rules();
        let mut result = Vec::new();

        // Recursive walker that stops descending into ignored directories.
        fn walk(
            root: &Path,
            dir: &Path,
            rules: &crate::ignore::IgnoreRules,
            out: &mut Vec<String>,
        ) {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let abs = entry.path();
                let rel = match abs.strip_prefix(root) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                // Never touch the .atomic directory itself.
                if rel.starts_with(DOT_DIR) {
                    continue;
                }

                let is_dir = abs.is_dir();

                if rules.is_ignored(rel, is_dir) {
                    // Collect the top-level ignored entry — don't recurse.
                    if let Some(s) = rel.to_str() {
                        out.push(s.to_string());
                    }
                } else if is_dir {
                    // Not ignored — recurse to find ignored children.
                    walk(root, &abs, rules, out);
                }
                // Non-ignored files are left alone.
            }
        }

        walk(&self.root, &self.root, &rules, &mut result);
        result
    }
}
