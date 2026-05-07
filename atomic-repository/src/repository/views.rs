use super::*;

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
    /// The number of changes inserted into this view
    pub change_count: u64,
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

        // Resolve parent name if the view has a parent
        let parent_name = if let Some(parent_id) = view.parent {
            txn.get_view_by_id(parent_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .map(|p| p.name)
        } else {
            None
        };

        Ok(ViewInfo {
            name: view.name.clone(),
            state: view.state,
            change_count: view.change_count,
            scope: view.kind,
            parent_name,
        })
    }
}
