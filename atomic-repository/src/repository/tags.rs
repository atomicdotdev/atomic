use super::*;

impl Repository {
    // Tag Methods

    /// Create a tag for the current state.
    ///
    /// Tags are named snapshots of a view's Merkle state. They can be
    /// lightweight (just name + state) or annotated (with message/author).
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name (must be valid per `validate_tag_name`)
    /// * `options` - Options for tag creation
    ///
    /// # Returns
    ///
    /// The created `Tag`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The tag name is invalid
    /// - A tag with this name already exists (unless `force` is set)
    /// - The view doesn't exist
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Create a lightweight tag
    /// let tag = repo.create_tag("v1.0.0", TagOptions::default())?;
    ///
    /// // Create an annotated tag
    /// let tag = repo.create_tag("v1.0.0", TagOptions::default()
    ///     .message("Release version 1.0.0")
    ///     .author("Alice", Some("alice@example.com")))?;
    /// ```
    pub fn create_tag(&self, name: &str, options: TagOptions) -> Result<Tag, RepositoryError> {
        // Validate the tag name
        validate_tag_name(name).map_err(|e| RepositoryError::InvalidTagName {
            name: name.to_string(),
            reason: e.to_string(),
        })?;

        // Get current view state
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view_name = options.view.as_deref().unwrap_or(&self.current_view);
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        // Determine sequence to tag
        let sequence = options
            .sequence
            .unwrap_or(view.change_count.saturating_sub(1));

        // Create the tag
        let tag = if options.is_annotated() {
            let message = options.message.unwrap_or_default();
            let author = options
                .author
                .unwrap_or_else(|| Author::new("Unknown", None::<String>));
            Tag::annotated(name, view_name, sequence, view.state, message, author)
        } else {
            Tag::new(name, view_name, sequence, view.state)
        };

        // Save to disk
        let tags_dir = self.dot_dir.join("tags");
        if options.force {
            save_tag_force(&tags_dir, &tag, true).map_err(|e| Self::convert_tag_error(e, name))?;
        } else {
            save_tag(&tags_dir, &tag).map_err(|e| Self::convert_tag_error(e, name))?;
        }

        Ok(tag)
    }

    /// Convert a TagError to a RepositoryError with proper variants.
    fn convert_tag_error(e: crate::tags::TagError, _name: &str) -> RepositoryError {
        match e {
            crate::tags::TagError::AlreadyExists { name } => {
                RepositoryError::TagAlreadyExists { name }
            }
            crate::tags::TagError::NotFound { name } => RepositoryError::TagNotFound { name },
            crate::tags::TagError::InvalidName { name, reason } => {
                RepositoryError::InvalidTagName { name, reason }
            }
            crate::tags::TagError::Io(e) => RepositoryError::Io(e),
            other => RepositoryError::Database(other.to_string()),
        }
    }

    /// Get a tag by name from the current view.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to look up
    ///
    /// # Returns
    ///
    /// The `Tag` if found, or `None` if not.
    pub fn get_tag(&self, name: &str) -> Result<Option<Tag>, RepositoryError> {
        self.get_tag_from_view(name, &self.current_view)
    }

    /// Get a tag by name from a specific view.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to look up
    /// * `view` - The view to search in
    ///
    /// # Returns
    ///
    /// The `Tag` if found, or `None` if not.
    pub fn get_tag_from_view(
        &self,
        name: &str,
        view: &str,
    ) -> Result<Option<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::load_tag(&tags_dir, view, name)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get a tag by name, searching all views.
    ///
    /// This is useful when you don't know which view a tag belongs to.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to look up
    ///
    /// # Returns
    ///
    /// The `Tag` if found in any view, or `None` if not.
    pub fn get_tag_any_view(&self, name: &str) -> Result<Option<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::load_tag_any_view(&tags_dir, name)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List all tags for the current view.
    ///
    /// # Returns
    ///
    /// A vector of tags in the current view.
    pub fn list_tags(&self) -> Result<Vec<Tag>, RepositoryError> {
        self.list_tags_for_view(&self.current_view)
    }

    /// List all tags for a specific view.
    ///
    /// # Arguments
    ///
    /// * `view` - The view to list tags from
    ///
    /// # Returns
    ///
    /// A vector of tags in the specified view.
    pub fn list_tags_for_view(&self, view: &str) -> Result<Vec<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::list_tags(&tags_dir, view)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List all tags across all views.
    ///
    /// # Returns
    ///
    /// A vector of all tags in the repository from all views.
    pub fn list_all_tags(&self) -> Result<Vec<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::list_all_tags(&tags_dir).map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List all views that have tags.
    ///
    /// # Returns
    ///
    /// A vector of view names that have at least one tag.
    pub fn list_tag_views(&self) -> Result<Vec<String>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::list_tag_views(&tags_dir).map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List tags matching a filter.
    ///
    /// # Arguments
    ///
    /// * `filter` - Filter criteria for tags
    ///
    /// # Returns
    ///
    /// A filtered and sorted vector of tags.
    pub fn list_tags_filtered(&self, filter: &TagFilter) -> Result<Vec<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::list_tags_filtered(&tags_dir, filter)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Delete a tag from the current view.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to delete
    ///
    /// # Returns
    ///
    /// `true` if the tag was deleted, `false` if it didn't exist.
    pub fn delete_tag(&self, name: &str) -> Result<bool, RepositoryError> {
        self.delete_tag_from_view(name, &self.current_view)
    }

    /// Delete a tag from a specific view.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to delete
    /// * `view` - The view to delete from
    ///
    /// # Returns
    ///
    /// `true` if the tag was deleted, `false` if it didn't exist.
    pub fn delete_tag_from_view(&self, name: &str, view: &str) -> Result<bool, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::delete_tag(&tags_dir, view, name)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count the number of tags in the current view.
    ///
    /// # Returns
    ///
    /// The number of tags in the current view.
    pub fn tag_count(&self) -> Result<usize, RepositoryError> {
        self.tag_count_for_view(&self.current_view)
    }

    /// Count the number of tags in a specific view.
    ///
    /// # Arguments
    ///
    /// * `view` - The view to count tags in
    ///
    /// # Returns
    ///
    /// The number of tags in the specified view.
    pub fn tag_count_for_view(&self, view: &str) -> Result<usize, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::count_tags(&tags_dir, view)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count all tags across all views.
    ///
    /// # Returns
    ///
    /// The total number of tags in the repository.
    pub fn tag_count_all(&self) -> Result<usize, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::count_all_tags(&tags_dir).map_err(|e| RepositoryError::Database(e.to_string()))
    }
}
