//! Tag query and listing operations.
//!
//! Functions for searching, listing, filtering, and counting tags
//! across one or more views.

use std::path::Path;

use super::types::{Tag, TagFilter, TagResult, TagSort};

/// Load a tag by name, searching all views.
///
/// This is useful when you don't know which view a tag belongs to.
/// Returns the first matching tag found.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `name` - The tag name
///
/// # Returns
///
/// The loaded `Tag`, or `None` if not found in any view.
pub fn load_tag_any_view(tags_dir: &Path, name: &str) -> TagResult<Option<Tag>> {
    if !tags_dir.exists() {
        return Ok(None);
    }

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let view = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            if let Some(tag) = super::load_tag(tags_dir, view, name)? {
                return Ok(Some(tag));
            }
        }
    }

    Ok(None)
}

/// List all tags for a specific view.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `view` - The view name
///
/// # Returns
///
/// A vector of loaded tags for the specified view.
pub fn list_tags(tags_dir: &Path, view: &str) -> TagResult<Vec<Tag>> {
    let view_dir = super::view_tags_dir(tags_dir, view);

    if !view_dir.exists() {
        return Ok(Vec::new());
    }

    let mut tags = Vec::new();

    for entry in std::fs::read_dir(&view_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "tag") {
            let contents = std::fs::read_to_string(&path)?;
            if let Ok(tag) = serde_json::from_str::<Tag>(&contents) {
                tags.push(tag);
            }
        }
    }

    Ok(tags)
}

/// List all tags across all views.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// A vector of all loaded tags from all views.
pub fn list_all_tags(tags_dir: &Path) -> TagResult<Vec<Tag>> {
    if !tags_dir.exists() {
        return Ok(Vec::new());
    }

    let mut tags = Vec::new();

    // Iterate over view directories
    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let view = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Get tags from this view
            let view_tags = list_tags(tags_dir, view)?;
            tags.extend(view_tags);
        }
    }

    Ok(tags)
}

/// List all view names that have tags.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// A vector of view names that have at least one tag.
pub fn list_tag_views(tags_dir: &Path) -> TagResult<Vec<String>> {
    if !tags_dir.exists() {
        return Ok(Vec::new());
    }

    let mut views = Vec::new();

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                views.push(name.to_string());
            }
        }
    }

    views.sort();
    Ok(views)
}

/// List tags matching a filter.
///
/// If the filter specifies a view, only that view is searched.
/// Otherwise, all views are searched.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `filter` - The filter to apply
///
/// # Returns
///
/// A filtered and sorted vector of tags.
pub fn list_tags_filtered(tags_dir: &Path, filter: &TagFilter) -> TagResult<Vec<Tag>> {
    // Get tags from appropriate source
    let all_tags = if let Some(ref view) = filter.view {
        list_tags(tags_dir, view)?
    } else {
        list_all_tags(tags_dir)?
    };

    let mut tags: Vec<Tag> = all_tags.into_iter().filter(|t| filter.matches(t)).collect();

    // Sort
    match filter.sort {
        TagSort::Name => tags.sort_by_key(|t| t.name.clone()),
        TagSort::Timestamp => tags.sort_by_key(|t| std::cmp::Reverse(t.timestamp)),
        TagSort::Sequence => tags.sort_by_key(|t| std::cmp::Reverse(t.sequence)),
    }

    // Limit
    if let Some(limit) = filter.limit {
        tags.truncate(limit);
    }

    Ok(tags)
}

/// Count tags for a specific view.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `view` - The view name
///
/// # Returns
///
/// The number of tags in the specified view.
pub fn count_tags(tags_dir: &Path, view: &str) -> TagResult<usize> {
    let view_dir = super::view_tags_dir(tags_dir, view);

    if !view_dir.exists() {
        return Ok(0);
    }

    let count = std::fs::read_dir(&view_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "tag"))
        .count();

    Ok(count)
}

/// Count all tags across all views.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// The total number of tags across all views.
pub fn count_all_tags(tags_dir: &Path) -> TagResult<usize> {
    if !tags_dir.exists() {
        return Ok(0);
    }

    let mut count = 0;

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let view = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            count += count_tags(tags_dir, view)?;
        }
    }

    Ok(count)
}
