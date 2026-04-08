//! Tag query and listing operations.
//!
//! Functions for searching, listing, filtering, and counting tags
//! across one or more stacks.

use std::path::Path;

use super::types::{Tag, TagFilter, TagResult, TagSort};

/// Load a tag by name, searching all stacks.
///
/// This is useful when you don't know which stack a tag belongs to.
/// Returns the first matching tag found.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `name` - The tag name
///
/// # Returns
///
/// The loaded `Tag`, or `None` if not found in any stack.
pub fn load_tag_any_stack(tags_dir: &Path, name: &str) -> TagResult<Option<Tag>> {
    if !tags_dir.exists() {
        return Ok(None);
    }

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let stack = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            if let Some(tag) = super::load_tag(tags_dir, stack, name)? {
                return Ok(Some(tag));
            }
        }
    }

    Ok(None)
}

/// List all tags for a specific stack.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `stack` - The stack name
///
/// # Returns
///
/// A vector of loaded tags for the specified stack.
pub fn list_tags(tags_dir: &Path, stack: &str) -> TagResult<Vec<Tag>> {
    let stack_dir = super::stack_tags_dir(tags_dir, stack);

    if !stack_dir.exists() {
        return Ok(Vec::new());
    }

    let mut tags = Vec::new();

    for entry in std::fs::read_dir(&stack_dir)? {
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

/// List all tags across all stacks.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// A vector of all loaded tags from all stacks.
pub fn list_all_tags(tags_dir: &Path) -> TagResult<Vec<Tag>> {
    if !tags_dir.exists() {
        return Ok(Vec::new());
    }

    let mut tags = Vec::new();

    // Iterate over stack directories
    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let stack = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Get tags from this stack
            let stack_tags = list_tags(tags_dir, stack)?;
            tags.extend(stack_tags);
        }
    }

    Ok(tags)
}

/// List all stack names that have tags.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// A vector of stack names that have at least one tag.
pub fn list_tag_stacks(tags_dir: &Path) -> TagResult<Vec<String>> {
    if !tags_dir.exists() {
        return Ok(Vec::new());
    }

    let mut stacks = Vec::new();

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                stacks.push(name.to_string());
            }
        }
    }

    stacks.sort();
    Ok(stacks)
}

/// List tags matching a filter.
///
/// If the filter specifies a stack, only that stack is searched.
/// Otherwise, all stacks are searched.
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
    let all_tags = if let Some(ref stack) = filter.stack {
        list_tags(tags_dir, stack)?
    } else {
        list_all_tags(tags_dir)?
    };

    let mut tags: Vec<Tag> = all_tags.into_iter().filter(|t| filter.matches(t)).collect();

    // Sort
    match filter.sort {
        TagSort::Name => tags.sort_by(|a, b| a.name.cmp(&b.name)),
        TagSort::Timestamp => tags.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
        TagSort::Sequence => tags.sort_by(|a, b| b.sequence.cmp(&a.sequence)),
    }

    // Limit
    if let Some(limit) = filter.limit {
        tags.truncate(limit);
    }

    Ok(tags)
}

/// Count tags for a specific stack.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `stack` - The stack name
///
/// # Returns
///
/// The number of tags in the specified stack.
pub fn count_tags(tags_dir: &Path, stack: &str) -> TagResult<usize> {
    let stack_dir = super::stack_tags_dir(tags_dir, stack);

    if !stack_dir.exists() {
        return Ok(0);
    }

    let count = std::fs::read_dir(&stack_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "tag"))
        .count();

    Ok(count)
}

/// Count all tags across all stacks.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// The total number of tags across all stacks.
pub fn count_all_tags(tags_dir: &Path) -> TagResult<usize> {
    if !tags_dir.exists() {
        return Ok(0);
    }

    let mut count = 0;

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let stack = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            count += count_tags(tags_dir, stack)?;
        }
    }

    Ok(count)
}
