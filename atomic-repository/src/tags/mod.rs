//! Tag management for Atomic VCS.
//!
//! Tags are named snapshots of a view's state at a particular point in time.
//! This module provides CRUD operations and query functions for tags.

mod queries;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public types and functions
pub use queries::{
    count_all_tags, count_tags, list_all_tags, list_tag_views, list_tags, list_tags_filtered,
    load_tag_any_view,
};
pub use types::{
    matches_pattern, validate_tag_name, Tag, TagError, TagFilter, TagOptions, TagResult, TagSort,
};

use std::path::{Path, PathBuf};

// Backward-compatible aliases
pub use queries::list_tag_views as list_tag_stacks;
pub use queries::load_tag_any_view as load_tag_any_stack;

// ============================================================================
// TAG FILE PATH HELPERS
// ============================================================================

/// Get the path for a tag file (per-view storage).
///
/// Tags are stored in a per-view directory structure:
/// `{tags_dir}/{view}/{name}.tag`
///
/// This allows the same tag name to exist in different views,
/// enabling view-specific releases and milestones.
pub fn tag_file_path(tags_dir: &Path, view: &str, name: &str) -> PathBuf {
    tags_dir.join(view).join(format!("{}.tag", name))
}

/// Get the view directory for tags.
pub fn view_tags_dir(tags_dir: &Path, view: &str) -> PathBuf {
    tags_dir.join(view)
}

/// Backward-compatible alias for [`view_tags_dir`].
pub fn stack_tags_dir(tags_dir: &Path, view: &str) -> PathBuf {
    view_tags_dir(tags_dir, view)
}

// ============================================================================
// SAVE OPERATIONS
// ============================================================================

/// Save a tag to a file (per-view storage).
///
/// The tag is saved to `{tags_dir}/{tag.view}/{tag.name}.tag`.
/// The view directory is created if it doesn't exist.
///
/// # Errors
///
/// Returns `TagError::AlreadyExists` if a tag with the same name
/// already exists in the same view.
pub fn save_tag(tags_dir: &Path, tag: &Tag) -> TagResult<()> {
    // Ensure view directory exists
    let view_dir = view_tags_dir(tags_dir, &tag.view);
    std::fs::create_dir_all(&view_dir)?;

    let path = tag_file_path(tags_dir, &tag.view, &tag.name);

    // Check for existing tag
    if path.exists() {
        return Err(TagError::AlreadyExists {
            name: tag.name.clone(),
        });
    }

    let contents =
        serde_json::to_string_pretty(tag).map_err(|e| TagError::Serialization(e.to_string()))?;

    std::fs::write(&path, contents)?;

    Ok(())
}

/// Save a tag to a file, optionally overwriting (per-view storage).
///
/// The tag is saved to `{tags_dir}/{tag.view}/{tag.name}.tag`.
pub fn save_tag_force(tags_dir: &Path, tag: &Tag, force: bool) -> TagResult<()> {
    // Ensure view directory exists
    let view_dir = view_tags_dir(tags_dir, &tag.view);
    std::fs::create_dir_all(&view_dir)?;

    let path = tag_file_path(tags_dir, &tag.view, &tag.name);

    // Check for existing tag
    if path.exists() && !force {
        return Err(TagError::AlreadyExists {
            name: tag.name.clone(),
        });
    }

    let contents =
        serde_json::to_string_pretty(tag).map_err(|e| TagError::Serialization(e.to_string()))?;

    std::fs::write(&path, contents)?;

    Ok(())
}

// ============================================================================
// LOAD OPERATIONS
// ============================================================================

/// Load a tag from a file (per-view storage).
///
/// # Returns
///
/// The loaded `Tag`, or `None` if not found.
pub fn load_tag(tags_dir: &Path, view: &str, name: &str) -> TagResult<Option<Tag>> {
    let path = tag_file_path(tags_dir, view, name);

    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&path)?;

    let tag: Tag = serde_json::from_str(&contents)
        .map_err(|_| TagError::InvalidTagFile { path: path.clone() })?;

    Ok(Some(tag))
}

// ============================================================================
// DELETE OPERATIONS
// ============================================================================

/// Delete a tag file (per-view storage).
///
/// # Returns
///
/// `Ok(true)` if deleted, `Ok(false)` if not found.
pub fn delete_tag(tags_dir: &Path, view: &str, name: &str) -> TagResult<bool> {
    let path = tag_file_path(tags_dir, view, name);

    if !path.exists() {
        return Ok(false);
    }

    std::fs::remove_file(&path)?;

    // Clean up empty view directory
    let view_dir = view_tags_dir(tags_dir, view);
    if view_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&view_dir) {
            if entries.count() == 0 {
                let _ = std::fs::remove_dir(&view_dir);
            }
        }
    }

    Ok(true)
}
