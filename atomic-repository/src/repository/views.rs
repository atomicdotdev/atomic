use super::*;

use std::collections::HashSet;

use crate::apply::InsertOptions;
use crate::manifest::ViewManifest;

/// Information about a view.
///
/// This struct provides metadata about a view including its current
/// Merkle state and the number of changes inserted into it.
#[derive(Debug, Clone)]
pub struct ViewInfo {
    /// The name of the view
    pub name: String,
    /// The current Merkle state (hash of all inserted changes)
    pub state: Merkle,
    /// Number of entries in this view's own VIEW_CHANGES log.
    ///
    /// This is the length of the view's own change sequence and is used to
    /// resolve change references (e.g. `@`, `@~1`). It is NOT the total
    /// effective change count for a draft — inherited changes are not stored
    /// in this view's log.
    pub change_count: u64,
    /// Number of this view's own changes that are unique to it (not visible
    /// through the parent chain). For shared views or views without a parent,
    /// this equals `change_count`.
    pub own_change_count: u64,
    /// Number of changes this view inherits through its parent chain. Zero for
    /// views without a parent.
    pub inherited_change_count: u64,
    /// View scope (Draft or Shared)
    pub scope: ViewScope,
    /// Parent view name, if any
    pub parent_name: Option<String>,
}

impl ViewInfo {
    /// Get the Merkle state as a base32-encoded string.
    pub fn state_base32(&self) -> String {
        self.state.to_base32()
    }

    /// Get a short version of the Merkle state (first 12 characters).
    pub fn state_short(&self) -> String {
        let full = self.state.to_base32();
        if full.len() > 12 {
            full[..12].to_string()
        } else {
            full
        }
    }

    /// Check if the view is empty (has no changes).
    pub fn is_empty(&self) -> bool {
        self.change_count == 0
    }

    /// Get a human-readable label for the view scope.
    pub fn kind_label(&self) -> &str {
        match self.scope {
            ViewScope::Shared => "shared",
            ViewScope::Draft => "draft",
        }
    }

    /// Get the parent name for display, or "—" if root.
    pub fn parent_display(&self) -> &str {
        self.parent_name.as_deref().unwrap_or("—")
    }
}

impl Repository {
    /// Create a new view.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the view to create
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view already exists
    /// - The database operation fails
    pub fn create_view(&mut self, name: &str) -> Result<(), RepositoryError> {
        // Create the workspace directory for this view.
        ensure_workspace_dir(&self.dot_dir, name)?;

        // Create a **Draft** view parented on the nearest Shared
        // ancestor of the current view.  The change log starts EMPTY —
        // no changes are inherited automatically.
        //
        // The parent link gives the view read-access to the shared
        // graph content (via the overlay chain) so that `record` can
        // compute diffs against the existing state.  But no files are
        // *materialised* on disk until changes are explicitly inserted
        // into this view (which copies them into the view's change log).
        //
        // This means:
        //   `view new feature`              → empty workspace, no files
        //   `insert from-view dev feature`  → inherits dev's files
        //
        // Using the nearest Shared ancestor (instead of the current
        // view directly) prevents sibling Draft views from seeing
        // each other's edges through the overlay chain.
        let parent_name = self.nearest_shared_ancestor(&self.current_view.clone())?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::ViewAlreadyExists {
                name: name.to_string(),
            });
        }

        let parent_view = txn
            .get_view(&parent_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: parent_name.clone(),
            })?;

        txn.create_view(name, ViewScope::Draft, Some(parent_view.id))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Create a new Shared view with no parent.
    ///
    /// Changes inserted into a Shared view go into the global `GRAPH` table
    /// and are permanently visible to all views. This is the correct scope
    /// to use for server-side push targets, where changes must be universally
    /// visible regardless of which run or request inserts them.
    ///
    /// Returns `ViewAlreadyExists` if the view already exists.
    pub fn create_shared_view(&mut self, name: &str) -> Result<(), RepositoryError> {
        ensure_workspace_dir(&self.dot_dir, name)?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::ViewAlreadyExists {
                name: name.to_string(),
            });
        }

        txn.create_view(name, ViewScope::Shared, None)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Change a view's scope between Draft and Shared.
    ///
    /// This is useful after `git import` which creates Draft views by
    /// default.
    pub fn set_view_scope(&self, name: &str, scope: ViewScope) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        view.kind = scope;
        // Clear parent when promoting to Shared root view
        if scope.is_shared() {
            view.parent = None;
        }

        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Repair an existing view's full identity (scope **and** parent) to match
    /// a declared manifest/snapshot.
    ///
    /// Unlike [`Repository::set_view_scope`], which only touches the scope
    /// (and clears the parent when promoting to Shared), this sets both fields
    /// atomically, resolving `parent_name` to its id. It is used on the sync
    /// path when a view was auto-created with the wrong identity (e.g. a draft
    /// that `ensure_view_exists` created as Shared before its snapshot was
    /// reconciled) so a subsequent `apply_view_manifest` no longer fails the
    /// identity check.
    ///
    /// Returns `ViewNotFound` if the view — or a named parent — does not exist.
    pub fn set_view_identity(
        &self,
        name: &str,
        scope: ViewScope,
        parent_name: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        let parent_id = match parent_name {
            Some(p) => Some(
                txn.get_view(p)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: p.to_string(),
                    })?
                    .id,
            ),
            None => None,
        };

        view.kind = scope;
        // A Shared view is a root; it never carries a parent.
        view.parent = if scope.is_shared() { None } else { parent_id };

        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Walk the parent chain from `view_name` and return the name of the
    /// first Shared view encountered.  If `view_name` is itself Shared,
    /// it is returned immediately.  This is used to determine the correct
    /// parent for newly created Draft views.
    pub fn nearest_shared_ancestor(&self, view_name: &str) -> Result<String, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        // Already Shared → use it directly.
        if view.kind.is_shared() {
            return Ok(view_name.to_string());
        }

        // Walk up the parent chain looking for a Shared ancestor.
        let mut cursor = view.parent;
        while let Some(parent_id) = cursor {
            if let Some(parent) = txn
                .get_view_by_id(parent_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                if parent.kind.is_shared() {
                    return Ok(parent.name.clone());
                }
                cursor = parent.parent;
            } else {
                break;
            }
        }

        // Fallback: if no Shared ancestor found (shouldn't happen in
        // normal use — dev is always Shared), use the current view.
        Ok(view_name.to_string())
    }

    /// Create a new view that inherits changes from another view.
    ///
    /// This creates a new view and copies all changes from the source view
    /// to the new view. The new view will have the same content state as
    /// the source view at the time of creation.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the new view to create
    /// * `from_view` - The name of the view to inherit changes from
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The new view already exists
    /// - The source view does not exist
    /// - The database operation fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Create a feature view that starts with dev's changes
    /// repo.create_view_from("feature", "dev")?;
    /// ```
    pub fn create_view_from(&mut self, name: &str, from_view: &str) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if the new view already exists
        if txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::ViewAlreadyExists {
                name: name.to_string(),
            });
        }

        // Get the source view
        let source_view = txn
            .get_view(from_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: from_view.to_string(),
            })?;

        let source_id = source_view.id;

        // Collect all changes from the source view
        let changes: Vec<(NodeId, Hash)> = {
            let iter = txn
                .iter_changes(&source_view, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            let mut result = Vec::new();
            for item in iter {
                let (_seq, node_id, _merkle) =
                    item.map_err(|e| RepositoryError::Database(e.to_string()))?;
                let hash = txn
                    .get_external(node_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| {
                        RepositoryError::Database(format!(
                            "Change {} has no external hash",
                            node_id.0
                        ))
                    })?;
                result.push((node_id, hash));
            }
            result
        };

        // Create the new view as a **Draft** view parented on the
        // source view.  Draft views write edges to GRAPH like all
        // views, but use a change filter for isolation. The parent
        // link means the view chain includes the source's content.
        // Create workspace directory for the new view.
        ensure_workspace_dir(&self.dot_dir, name)?;

        let mut new_view = txn
            .create_view(name, ViewScope::Draft, Some(source_id))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Copy all changes from the source to the new view's log.
        // This does NOT re-insert hunks — the edges already exist in
        // GRAPH. The new view sees them via the change filter.
        for (node_id, hash) in changes {
            txn.put_change(&mut new_view, node_id, &hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Update the view state
        txn.update_view(&new_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// List all views in the repository.
    ///
    /// # Returns
    ///
    /// A vector of view names.
    pub fn list_views(&self) -> Result<Vec<String>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.list_views()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Return the names of all views whose change-set references `hash`.
    pub fn views_containing_change(&self, hash: &Hash) -> Result<Vec<String>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let change_id = match txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(id) => id,
            None => return Ok(vec![]),
        };

        let mut names = Vec::new();
        for name in txn
            .list_views()
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let view = match txn
                .get_view(&name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                Some(view) => view,
                None => continue,
            };
            if txn
                .get_change_seq(&view, change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .is_some()
            {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// Check if a view exists.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the view to check
    pub fn view_exists(&self, name: &str) -> Result<bool, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some())
    }

    /// Delete a view from the repository.
    ///
    /// This removes the view and all its associated metadata, but does not
    /// delete the changes themselves. Changes remain in the graph and may be
    /// referenced by other views.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the view to delete
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view does not exist
    /// - The view is the current view (cannot delete current view)
    /// - The database operation fails
    pub fn delete_view(&mut self, name: &str) -> Result<(), RepositoryError> {
        // Cannot delete the current view
        if name == self.current_view {
            return Err(RepositoryError::CannotDeleteCurrentView {
                name: name.to_string(),
            });
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the view to delete
        let view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        // Delete the view.
        //
        // `del_view` enforces:
        // - Shared views cannot be deleted (returns CannotDeleteSharedView)
        // - Views with children cannot be deleted (returns ViewHasChildren)
        // Remove workspace directory for this view before deleting
        // the view from the database.  This cleans up any shelved
        // artifacts (node_modules, dist, etc.) that were stored when
        // the user last switched away from this view.
        let ws = workspace_path(&self.dot_dir, name);
        if ws.is_dir() {
            let _ = std::fs::remove_dir_all(&ws);
        }

        txn.del_view(&view).map_err(|e| match &e {
            atomic_core::pristine::PristineError::CannotDeleteSharedView { name } => {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot delete shared view '{}': shared views are permanent. \
                         Use 'view new' to create a draft view instead.",
                        name
                    ),
                }
            }
            atomic_core::pristine::PristineError::ViewHasChildren { name, children } => {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot delete view '{}': has child views ({}). \
                         Delete or reparent children first.",
                        name,
                        children.join(", ")
                    ),
                }
            }
            _ => RepositoryError::Database(e.to_string()),
        })?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Get the parent view's change count for a given view.
    ///
    /// Returns `Some((parent_name, parent_change_count))` if the view has a
    /// parent, or `None` if it is a root view.
    ///
    /// This is used by `atomic log` to determine the fork point: changes
    /// with sequence numbers `>= parent_change_count` are local to the
    /// draft view, while those below were inherited when the view was
    /// created.
    pub fn parent_change_count(
        &self,
        view_name: &str,
    ) -> Result<Option<(String, u64)>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        match view.parent {
            Some(parent_id) => {
                let parent = txn
                    .get_view_by_id(parent_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: format!("<parent id {}>", parent_id),
                    })?;
                Ok(Some((parent.name.clone(), parent.change_count)))
            }
            None => Ok(None),
        }
    }

    /// Get information about a view.
    ///
    /// Returns the view's metadata including its Merkle state and change count.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the view to query
    ///
    /// # Returns
    ///
    /// A `ViewInfo` struct with the view's metadata, or an error if the view
    /// doesn't exist.
    pub fn get_view_info(&self, name: &str) -> Result<ViewInfo, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        // Resolve parent name and compute own/inherited change counts by
        // actual graph membership rather than by assuming the own log is a
        // superset of the parent (which only holds for `create_view_from`
        // drafts, not for record- or split-created drafts).
        let (parent_name, own_change_count, inherited_change_count) = match view.parent {
            Some(parent_id) => {
                match txn
                    .get_view_by_id(parent_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                {
                    Some(parent) => {
                        let parent_visible = collect_visible_change_ids(&txn, &parent)?;
                        let own_ids = collect_view_change_ids(&txn, &view)?;
                        let own = own_ids.difference(&parent_visible).count() as u64;
                        (Some(parent.name), own, parent_visible.len() as u64)
                    }
                    None => (None, view.change_count, 0),
                }
            }
            None => (None, view.change_count, 0),
        };

        Ok(ViewInfo {
            name: view.name.clone(),
            state: view.state,
            change_count: view.change_count,
            own_change_count,
            inherited_change_count,
            scope: view.kind,
            parent_name,
        })
    }

    /// Create a new Draft view parented on an explicit named parent.
    ///
    /// Unlike [`Repository::create_view`], which parents on the nearest
    /// shared ancestor of the *current* view, this method parents on the
    /// given view regardless of what is currently checked out. The new
    /// view's change log starts empty.
    ///
    /// Returns `ViewNotFound` if the parent does not exist and
    /// `ViewAlreadyExists` if the name is taken.
    pub fn create_draft_view(
        &mut self,
        name: &str,
        parent_name: &str,
    ) -> Result<(), RepositoryError> {
        self.create_view_with_identity(name, ViewScope::Draft, Some(parent_name))
    }

    /// Create a view with an explicit scope and optional named parent.
    ///
    /// The view's change log starts empty; this only establishes identity.
    fn create_view_with_identity(
        &mut self,
        name: &str,
        scope: ViewScope,
        parent_name: Option<&str>,
    ) -> Result<(), RepositoryError> {
        ensure_workspace_dir(&self.dot_dir, name)?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::ViewAlreadyExists {
                name: name.to_string(),
            });
        }

        let parent_id = match parent_name {
            Some(p) => Some(
                txn.get_view(p)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: p.to_string(),
                    })?
                    .id,
            ),
            None => None,
        };

        txn.create_view(name, scope, parent_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Export a view's complete identity as a [`ViewManifest`].
    ///
    /// The manifest carries the view's change log **exactly as stored** in
    /// `VIEW_CHANGES` (for a draft this includes the inherited prefix copied
    /// at fork time), plus scope, parent name, and the view's merkle state.
    /// The exported manifest is verified before it is returned, so a
    /// corrupted log surfaces here rather than on the receiving end.
    pub fn view_manifest(&self, name: &str) -> Result<ViewManifest, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        let parent = match view.parent {
            Some(parent_id) => txn
                .get_view_by_id(parent_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .map(|p| p.name),
            None => None,
        };

        let mut changes = Vec::with_capacity(view.change_count as usize);
        for item in txn
            .iter_changes(&view, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let (_seq, node_id, _merkle) =
                item.map_err(|e| RepositoryError::Database(e.to_string()))?;
            let hash = txn
                .get_external(node_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "change {} in view '{}' has no external hash",
                        node_id.0, name
                    ))
                })?;
            changes.push(hash);
        }

        let manifest = ViewManifest {
            name: view.name.clone(),
            scope: view.kind,
            parent,
            changes,
            state: view.state,
        };
        manifest.verify()?;
        Ok(manifest)
    }

    /// Reconcile a view manifest by **set union** over the common causal graph.
    ///
    /// Normal synchronization is monotonic: independently-added patches in a
    /// shared view commute, so neither ordered log replaces the other. This
    /// method preserves the local own membership and adds every incoming own
    /// change not already present. For drafts, inherited membership remains
    /// derived from the already-reconciled parent metadata; only the draft's
    /// own changes are unioned here.
    ///
    /// The caller applies ancestor manifests root-to-leaf. Every referenced
    /// `.change` must already be stored; `insert_change` registers/applies a
    /// graph node only when absent and otherwise performs the metadata write.
    /// Identity (scope + parent) is immutable and must match.
    pub fn reconcile_view_manifest(
        &mut self,
        manifest: &ViewManifest,
    ) -> Result<ManifestApplyOutcome, RepositoryError> {
        manifest.verify()?;

        for hash in &manifest.changes {
            if !self.has_change(hash) {
                return Err(RepositoryError::ManifestMissingChanges {
                    view: manifest.name.clone(),
                    count: 1,
                    first: hash.to_base32(),
                });
            }
        }

        if self.view_exists(&manifest.name)? {
            let info = self.get_view_info(&manifest.name)?;
            if info.scope != manifest.scope {
                return Err(RepositoryError::ManifestIdentityMismatch {
                    view: manifest.name.clone(),
                    reason: format!(
                        "local scope is {:?}, manifest declares {:?}",
                        info.scope, manifest.scope
                    ),
                });
            }
            if info.parent_name != manifest.parent {
                return Err(RepositoryError::ManifestIdentityMismatch {
                    view: manifest.name.clone(),
                    reason: format!(
                        "local parent is {:?}, manifest declares {:?}",
                        info.parent_name, manifest.parent
                    ),
                });
            }
        } else {
            self.create_view_with_identity(
                &manifest.name,
                manifest.scope,
                manifest.parent.as_deref(),
            )
            .map_err(|e| match e {
                RepositoryError::ViewNotFound { name } => RepositoryError::ManifestParentMissing {
                    view: manifest.name.clone(),
                    parent: name,
                },
                other => other,
            })?;
        }

        let local = self.view_manifest(&manifest.name)?;
        let mut present: HashSet<Hash> = local.changes.iter().copied().collect();
        let already_present = manifest
            .changes
            .iter()
            .filter(|hash| present.contains(hash))
            .count();
        let mut replayed = 0usize;
        for hash in &manifest.changes {
            if present.insert(*hash) {
                self.insert_change(hash, InsertOptions::default().view(&manifest.name))?;
                replayed += 1;
            }
        }

        let info = self.get_view_info(&manifest.name)?;
        Ok(ManifestApplyOutcome {
            view: manifest.name.clone(),
            already_present,
            replayed,
            state: info.state,
        })
    }

    /// Declaratively apply a [`ViewManifest`]: create the view with its
    /// declared identity if absent, then fast-forward its change log to
    /// match the manifest.
    ///
    /// # Semantics
    ///
    /// Everything is validated **before** any write:
    ///
    /// 1. The manifest's declared state must equal the fold of its log.
    /// 2. Every referenced change file must be present in the local store.
    /// 3. Every dependency of every change must appear earlier in the log
    ///    or already be applied locally (dependency truth is read from the
    ///    change files themselves, never from sender claims).
    /// 4. If the view exists, its scope and parent must match the manifest
    ///    and its log must be a prefix of the manifest log; anything else
    ///    is a divergence error, never a silent merge.
    /// 5. If the view is absent, the declared parent must already exist
    ///    (apply manifests root → leaf).
    ///
    /// Replay is prefix-resumable, not single-transaction: each change
    /// application is individually atomic, so an interrupted apply leaves
    /// the view at a valid earlier prefix and re-applying the same manifest
    /// resumes where it stopped. After replay the view's merkle state is
    /// verified against the declared state.
    pub fn apply_view_manifest(
        &mut self,
        manifest: &ViewManifest,
    ) -> Result<ManifestApplyOutcome, RepositoryError> {
        // 1. Structural integrity: declared state == fold of the log.
        manifest.verify()?;

        // 2. Presence: every change file must exist locally.
        let missing: Vec<&Hash> = manifest
            .changes
            .iter()
            .filter(|h| !self.has_change(h))
            .collect();
        if let Some(first) = missing.first() {
            return Err(RepositoryError::ManifestMissingChanges {
                view: manifest.name.clone(),
                count: missing.len(),
                first: first.to_base32(),
            });
        }

        // 3. Dependency closure: deps must be earlier in the log or already
        //    applied locally. Read from the change files (self-contained DAG).
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut seen: HashSet<Hash> = HashSet::with_capacity(manifest.changes.len());
            for hash in &manifest.changes {
                let change = self.load_change(hash)?;
                for dep in change.dependencies() {
                    if seen.contains(dep) {
                        continue;
                    }
                    let applied = txn
                        .get_internal(dep)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                        .is_some();
                    if !applied {
                        return Err(RepositoryError::ManifestDependencyMissing {
                            view: manifest.name.clone(),
                            change: hash.to_base32(),
                            dependency: dep.to_base32(),
                        });
                    }
                }
                seen.insert(*hash);
            }
        }

        // 4. Identity + prefix rule against the existing view (if any).
        let prefix_len = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            match txn
                .get_view(&manifest.name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                Some(view) => {
                    if view.kind != manifest.scope {
                        return Err(RepositoryError::ManifestIdentityMismatch {
                            view: manifest.name.clone(),
                            reason: format!(
                                "local scope is {:?}, manifest declares {:?}",
                                view.kind, manifest.scope
                            ),
                        });
                    }
                    let local_parent = match view.parent {
                        Some(pid) => txn
                            .get_view_by_id(pid)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?
                            .map(|p| p.name),
                        None => None,
                    };
                    if local_parent != manifest.parent {
                        return Err(RepositoryError::ManifestIdentityMismatch {
                            view: manifest.name.clone(),
                            reason: format!(
                                "local parent is {:?}, manifest declares {:?}",
                                local_parent, manifest.parent
                            ),
                        });
                    }

                    // Local log must be a prefix of the manifest log.
                    let mut local_len = 0usize;
                    for item in txn
                        .iter_changes(&view, 0)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                    {
                        let (_seq, node_id, _merkle) =
                            item.map_err(|e| RepositoryError::Database(e.to_string()))?;
                        let local_hash = txn
                            .get_external(node_id)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?
                            .ok_or_else(|| {
                                RepositoryError::Database(format!(
                                    "change {} in view '{}' has no external hash",
                                    node_id.0, manifest.name
                                ))
                            })?;
                        match manifest.changes.get(local_len) {
                            Some(expected) if *expected == local_hash => local_len += 1,
                            _ => {
                                return Err(RepositoryError::ManifestDiverged {
                                    view: manifest.name.clone(),
                                    at: local_len as u64,
                                })
                            }
                        }
                    }
                    local_len
                }
                None => 0,
            }
        };

        // 5. Create the view with its declared identity if absent.
        if prefix_len == 0 && !self.view_exists(&manifest.name)? {
            match self.create_view_with_identity(
                &manifest.name,
                manifest.scope,
                manifest.parent.as_deref(),
            ) {
                Ok(()) => {}
                Err(RepositoryError::ViewNotFound { name }) => {
                    return Err(RepositoryError::ManifestParentMissing {
                        view: manifest.name.clone(),
                        parent: name,
                    })
                }
                Err(e) => return Err(e),
            }
        }

        // 6. Replay the suffix in exact log order. insert_change appends to
        //    this view's log (put_change) and skips hunk application for
        //    changes already in the global graph, so replaying a draft's
        //    inherited prefix is a metadata-only operation.
        let mut replayed = 0usize;
        for hash in &manifest.changes[prefix_len..] {
            let options = InsertOptions::default().view(&manifest.name);
            self.insert_change(hash, options)?;
            replayed += 1;
        }

        // 7. End-to-end verification: the view's state must now equal the
        //    declared merkle.
        let info = self.get_view_info(&manifest.name)?;
        if info.state != manifest.state {
            return Err(RepositoryError::ManifestStateMismatch {
                view: manifest.name.clone(),
                declared: manifest.state.to_base32(),
                actual: info.state.to_base32(),
            });
        }

        Ok(ManifestApplyOutcome {
            view: manifest.name.clone(),
            already_present: prefix_len,
            replayed,
            state: info.state,
        })
    }
}

/// Outcome of [`Repository::apply_view_manifest`].
#[derive(Debug, Clone)]
pub struct ManifestApplyOutcome {
    /// The view the manifest was applied to.
    pub view: String,
    /// Log entries that were already present locally (the matched prefix).
    pub already_present: usize,
    /// Log entries replayed by this apply.
    pub replayed: usize,
    /// The view's merkle state after apply (equals the declared state).
    pub state: Merkle,
}
