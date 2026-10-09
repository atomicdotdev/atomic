//! Pre-filter logic for view-aware repository materialization.
//!
//! When `graph_visibility` is active (view-aware output), these functions
//! compute which file paths and ancestor directories should be included
//! in the output. This prevents recreating directories and empty files
//! that belong to a different view.

use std::collections::HashSet;

use crate::pristine::GraphVisibilityClosure;
use crate::types::NodeId;

pub use super::types::OutputItem;

/// Compute both the passing file paths and their ancestor directories.
///
/// When no graph visibility closure is active, returns `(None, None)` — meaning
/// all files and directories should be included.
///
/// # Arguments
///
/// * `items` - All output items (files and directories)
/// * `graph_visibility` - Optional validated dependency closure for the current view
///
/// # Returns
///
/// A tuple of `(Option<passing_file_paths>, Option<passing_ancestors>)`.
pub fn compute_filters(
    items: &[OutputItem],
    graph_visibility: Option<&GraphVisibilityClosure>,
) -> (Option<HashSet<String>>, Option<HashSet<String>>) {
    match graph_visibility {
        Some(visibility) => {
            let paths = compute_passing_file_paths(items, visibility);
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

/// Compute the set of repository paths whose introducing change passes the filter.
///
/// Files and graph-backed explicit directories participate. Synthetic ancestor
/// directories use ROOT and are included only when they are ancestors of a
/// passing graph-backed item.
///
/// # Arguments
///
/// * `items` - All output items (files and directories)
/// * `visibility` - The validated dependency closure visible in the current view
///
/// # Returns
///
/// A `HashSet` of file path strings that pass the filter.
pub fn compute_passing_file_paths(
    items: &[OutputItem],
    visibility: &GraphVisibilityClosure,
) -> HashSet<String> {
    let mut paths = HashSet::new();
    for item in items {
        let change_id = item.position.change;
        if item.is_directory && change_id == NodeId::ROOT {
            continue;
        }
        if change_id == NodeId::ROOT || visibility.contains(change_id) {
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
