//! Tag management for Atomic VCS.
//!
//! Tags are named snapshots of a stack's state at a particular point in time.
//! This module provides CRUD operations and query functions for tags.

mod queries;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public types and functions
pub use queries::{
    count_all_tags, count_tags, list_all_tags, list_tag_stacks, list_tags, list_tags_filtered,
    load_tag_any_stack,
};
pub use types::{
    matches_pattern, validate_tag_name, Tag, TagError, TagFilter, TagOptions, TagResult, TagSort,
};

use std::path::{Path, PathBuf};

// ============================================================================
// TAG FILE PATH HELPERS
// ============================================================================

/// Get the path for a tag file (per-stack storage).
///
/// Tags are stored in a per-stack directory structure:
/// `{tags_dir}/{stack}/{name}.tag`
///
/// This allows the same tag name to exist in different stacks,
/// enabling stack-specific releases and milestones.
pub fn tag_file_path(tags_dir: &Path, stack: &str, name: &str) -> PathBuf {
    tags_dir.join(stack).join(format!("{}.tag", name))
}

/// Get the stack directory for tags.
pub fn stack_tags_dir(tags_dir: &Path, stack: &str) -> PathBuf {
    tags_dir.join(stack)
}

// ============================================================================
// SAVE OPERATIONS
// ============================================================================

/// Save a tag to a file (per-stack storage).
///
/// The tag is saved to `{tags_dir}/{tag.stack}/{tag.name}.tag`.
/// The stack directory is created if it doesn't exist.
///
/// # Errors
///
/// Returns `TagError::AlreadyExists` if a tag with the same name
/// already exists in the same stack.
pub fn save_tag(tags_dir: &Path, tag: &Tag) -> TagResult<()> {
    // Ensure stack directory exists
    let stack_dir = stack_tags_dir(tags_dir, &tag.stack);
    std::fs::create_dir_all(&stack_dir)?;

    let path = tag_file_path(tags_dir, &tag.stack, &tag.name);

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

/// Save a tag to a file, optionally overwriting (per-stack storage).
///
/// The tag is saved to `{tags_dir}/{tag.stack}/{tag.name}.tag`.
pub fn save_tag_force(tags_dir: &Path, tag: &Tag, force: bool) -> TagResult<()> {
    // Ensure stack directory exists
    let stack_dir = stack_tags_dir(tags_dir, &tag.stack);
    std::fs::create_dir_all(&stack_dir)?;

    let path = tag_file_path(tags_dir, &tag.stack, &tag.name);

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

/// Load a tag from a file (per-stack storage).
///
/// # Returns
///
/// The loaded `Tag`, or `None` if not found.
pub fn load_tag(tags_dir: &Path, stack: &str, name: &str) -> TagResult<Option<Tag>> {
    let path = tag_file_path(tags_dir, stack, name);

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

/// Delete a tag file (per-stack storage).
///
/// # Returns
///
/// `Ok(true)` if deleted, `Ok(false)` if not found.
pub fn delete_tag(tags_dir: &Path, stack: &str, name: &str) -> TagResult<bool> {
    let path = tag_file_path(tags_dir, stack, name);

    if !path.exists() {
        return Ok(false);
    }

    std::fs::remove_file(&path)?;

    // Clean up empty stack directory
    let stack_dir = stack_tags_dir(tags_dir, stack);
    if stack_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&stack_dir) {
            if entries.count() == 0 {
                let _ = std::fs::remove_dir(&stack_dir);
            }
        }
    }

    Ok(true)
}
