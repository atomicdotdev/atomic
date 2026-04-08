//! Pre-filter logic for stack-aware repository materialization.
//!
//! When a `change_filter` is active (stack-aware output), these functions
//! compute which file paths and ancestor directories should be included
//! in the output. This prevents recreating directories and empty files
//! that belong to a different stack.

use std::collections::HashSet;
use std::sync::Arc;

use crate::types::NodeId;

pub use super::types::OutputItem;

/// Compute both the passing file paths and their ancestor directories.
///
/// When no `change_filter` is active, returns `(None, None)` — meaning
/// all files and directories should be included.
///
/// # Arguments
///
/// * `items` - All output items (files and directories)
/// * `change_filter` - Optional set of change NodeIds visible in the current view
///
/// # Returns
///
/// A tuple of `(Option<passing_file_paths>, Option<passing_ancestors>)`.
pub fn compute_filters(
    items: &[OutputItem],
    change_filter: &Option<Arc<HashSet<NodeId>>>,
) -> (Option<HashSet<String>>, Option<HashSet<String>>) {
    match change_filter {
        Some(filter) => {
            let paths = compute_passing_file_paths(items, filter);
            let ancestors = compute_passing_ancestors(&paths);
            (Some(paths), Some(ancestors))
        }
        None => (None, None),
    }
}

/// Check whether a directory is an ancestor of at least one passing file.
///
/// When no filter is active (`passing_ancestors` is `None`), all directories
/// are considered passing (returns `true`).
pub fn dir_has_passing_children(
    dir_path: &str,
    passing_ancestors: &Option<HashSet<String>>,
) -> bool {
    match passing_ancestors {
        None => true, // No filter — always create directories
        Some(ancestors) => ancestors.contains(dir_path),
    }
}

/// Compute the set of file paths whose introducing change passes the filter.
///
/// Only non-directory items whose `position.change` is either ROOT or
/// present in the filter set are included.
///
/// # Arguments
///
/// * `items` - All output items (files and directories)
/// * `filter` - The set of change NodeIds that are visible in the current view
///
/// # Returns
///
/// A `HashSet` of file path strings that pass the filter.
pub fn compute_passing_file_paths(
    items: &[OutputItem],
    filter: &Arc<HashSet<NodeId>>,
) -> HashSet<String> {
    let mut paths = HashSet::new();
    for item in items {
        if item.is_directory {
            continue;
        }
        // Check whether the file's introducing change is in the filter.
        // position.change gives us the NodeId of the change that created
        // this file's inode vertex.
        let change_id = item.position.change;
        if change_id == NodeId::ROOT || filter.contains(&change_id) {
            paths.insert(item.path.clone());
        }
    }
    paths
}

/// Compute the set of ancestor directory paths from a set of passing file paths.
///
/// This turns the directory visibility check from O(dirs × files) into
/// O(1) per directory via `HashSet` lookup.
fn compute_passing_ancestors(paths: &HashSet<String>) -> HashSet<String> {
    let mut ancestors = HashSet::new();
    for path in paths {
        let p = std::path::Path::new(path);
        // Walk every ancestor of this file path and record it.
        for ancestor in p.ancestors() {
            let s = match ancestor.to_str() {
                Some(s) if !s.is_empty() && s != "." => s,
                _ => break,
            };
            ancestors.insert(s.to_string());
        }
    }
    ancestors
}
