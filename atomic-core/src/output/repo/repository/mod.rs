//! Full repository output to working copy.
//!
//! Orchestrates tree traversal, file output, and conflict collection
//! for materializing the repository graph state to the working copy.

mod filter;
mod types;

#[cfg(test)]
mod tests;

pub use types::{MaterializeError, MaterializeOptions, OutputItem};

use std::collections::BTreeMap;

use crate::change::ChangeStore;
use crate::output::traits::WorkingCopy;
use crate::pristine::{GraphTxnT, TreeTxnT};
use crate::types::Inode;

use super::conflict::{FileConflict, FileConflictType};
use super::file::{output_file_with_filter, FileOutputResult};
use super::outcome::OutputOutcome;
use super::tree::{collect_tree, TreeCollectOptions};

// ============================================================================
// MATERIALIZE RESULT
// ============================================================================

/// Result of a materialize operation.
///
/// Contains statistics about the output and all conflicts detected.
#[derive(Debug, Clone, Default)]
pub struct MaterializeResult {
    /// Number of files written.
    pub files_written: usize,
    /// Number of files skipped (due to mtime or prefix filter).
    pub files_skipped: usize,
    /// Number of directories created.
    pub directories_created: usize,
    /// Total bytes written across all files.
    pub bytes_written: u64,
    /// Total vertices processed across all files.
    pub vertices_processed: usize,
    /// Total edges traversed across all files.
    pub edges_traversed: usize,
    /// Number of files that were truncated due to max_vertices.
    pub files_truncated: usize,
    /// All conflicts detected during output.
    pub conflicts: Vec<FileConflict>,
    /// Per-file results (optional, for detailed reporting).
    pub file_results: BTreeMap<String, FileOutputResult>,
}

impl MaterializeResult {
    /// Create a new empty result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any conflicts were detected.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Get the total number of conflicts.
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// Get conflicts filtered by type.
    pub fn conflicts_of_type(
        &self,
        conflict_type: FileConflictType,
    ) -> impl Iterator<Item = &FileConflict> {
        self.conflicts
            .iter()
            .filter(move |c| c.conflict_type == conflict_type)
    }

    /// Get name conflicts only.
    pub fn name_conflicts(&self) -> impl Iterator<Item = &FileConflict> {
        self.conflicts_of_type(FileConflictType::Name)
    }

    /// Get content conflicts only (Order, Cyclic, Zombie).
    pub fn content_conflicts(&self) -> impl Iterator<Item = &FileConflict> {
        self.conflicts.iter().filter(|c| c.is_content_conflict())
    }

    /// Add a conflict to the result.
    pub fn add_conflict(&mut self, conflict: FileConflict) {
        self.conflicts.push(conflict);
    }

    /// Merge results from a file output.
    pub fn merge_file_result(&mut self, file_result: FileOutputResult, store_result: bool) {
        self.files_written += 1;
        self.bytes_written += file_result.bytes_written;
        self.vertices_processed += file_result.vertices_processed;
        self.edges_traversed += file_result.edges_traversed;

        if file_result.was_truncated {
            self.files_truncated += 1;
        }

        for conflict in file_result.conflicts.iter() {
            self.conflicts.push(conflict.clone());
        }

        if store_result {
            self.file_results
                .insert(file_result.path.clone(), file_result);
        }
    }

    /// Record that a file was skipped.
    pub fn record_skipped(&mut self) {
        self.files_skipped += 1;
    }

    /// Record that a directory was created.
    pub fn record_directory(&mut self) {
        self.directories_created += 1;
    }

    /// Convert to OutputOutcome for unified result type.
    pub fn to_outcome(&self) -> OutputOutcome {
        let mut outcome = OutputOutcome::new();

        for _ in 0..self.files_written {
            outcome.record_file("", 0);
        }

        for i in 0..self.directories_created {
            outcome.record_directory(format!("dir_{}", i));
        }

        for _ in 0..self.files_skipped {
            outcome.record_skip("");
        }

        // Set total bytes (overwrite the 0s from record_file calls)
        outcome.bytes_written = self.bytes_written;

        outcome
    }
}

// ============================================================================
// COLLECT CHILDREN
// ============================================================================

/// Collect children of a directory from the graph.
///
/// Uses the tree traversal module to collect all files and directories
/// under the specified parent path, applying the repository output options
/// for filtering.
pub fn collect_children<T: TreeTxnT + GraphTxnT>(
    txn: &T,
    _parent_inode: Inode,
    parent_path: &str,
    options: &MaterializeOptions,
) -> Result<Vec<OutputItem>, crate::pristine::PristineError> {
    // Convert MaterializeOptions to TreeCollectOptions
    let mut tree_opts = TreeCollectOptions::new()
        .collect_directories(true)
        .collect_files(true);

    // Apply prefix filter
    if !options.prefix.is_empty() {
        tree_opts = tree_opts.prefix(&options.prefix);
    }

    // Collect from tree
    let tree_result = collect_tree(txn, parent_path, tree_opts)?;

    // Convert TreeItems to OutputItems
    let items = tree_result
        .items
        .into_iter()
        .map(|tree_item| {
            if tree_item.is_directory {
                OutputItem::directory(tree_item.path, tree_item.inode)
                    .with_metadata(tree_item.metadata)
            } else {
                OutputItem::file(tree_item.path, tree_item.inode, tree_item.position)
                    .with_metadata(tree_item.metadata)
            }
        })
        .collect();

    Ok(items)
}

// ============================================================================
// MATERIALIZE VIEW
// ============================================================================

/// Materialize the repository view (or prefix) to the working copy.
///
/// This is the main entry point for synchronizing the working copy with
/// the repository graph state. It traverses the tree, outputs each file,
/// and collects all conflicts.
pub fn materialize_view<T, C, W>(
    txn: &T,
    changes: &C,
    working_copy: &W,
    options: MaterializeOptions,
) -> Result<MaterializeResult, MaterializeError<W::Error>>
where
    T: TreeTxnT + GraphTxnT,
    C: ChangeStore,
    W: WorkingCopy,
    W::Writer: std::io::Write,
{
    let mut result = MaterializeResult::new();

    // Collect items to output starting from root
    let items = collect_children(txn, Inode::ROOT, "", &options)?;

    // Process each item
    let file_options = options.to_file_options();

    // ── View-aware pre-filter ──────────────────────────────────────
    let (passing_file_paths, passing_ancestors) =
        filter::compute_filters(&items, &options.change_filter);

    for item in items {
        if item.is_directory {
            // Only create directories that will contain files on this view
            if !filter::dir_has_passing_children(&item.path, &passing_ancestors) {
                result.record_skipped();
                continue;
            }
            // Selective materialize: skip directories with no target files.
            if let Some(ref paths) = options.only_paths {
                let has_target = paths.iter().any(|p| p.starts_with(&item.path));
                if !has_target {
                    result.record_skipped();
                    continue;
                }
            }
            // Create directory
            working_copy
                .create_dir_all(&item.path)
                .map_err(MaterializeError::WorkingCopy)?;
            result.record_directory();
        } else {
            // Check prefix filter
            if !options.matches_prefix(&item.path) {
                result.record_skipped();
                continue;
            }

            // Selective materialize: skip files not in the explicit path set.
            if let Some(ref paths) = options.only_paths {
                if !paths.contains(&item.path) {
                    result.record_skipped();
                    continue;
                }
            }

            // View-aware skip: if the file's introducing change is not in the
            // filter, skip it entirely.
            if let Some(ref paths) = passing_file_paths {
                if !paths.contains(&item.path) {
                    result.record_skipped();
                    continue;
                }
            }

            // Output file
            match output_file_with_filter(
                txn,
                changes,
                working_copy,
                item.inode,
                item.position,
                &item.path,
                file_options,
                options.change_filter.clone(),
            ) {
                Ok(file_result) => {
                    result.merge_file_result(file_result, false);
                }
                Err(e) => {
                    // Log error but continue with other files
                    log::warn!("Failed to output {}: {:?}", item.path, e);
                }
            }
        }
    }

    Ok(result)
}

/// Materialize a specific prefix of the repository to the working copy.
///
/// Convenience function that sets the prefix option.
pub fn materialize_prefix<T, C, W>(
    txn: &T,
    changes: &C,
    working_copy: &W,
    prefix: &str,
) -> Result<MaterializeResult, MaterializeError<W::Error>>
where
    T: TreeTxnT + GraphTxnT,
    C: ChangeStore,
    W: WorkingCopy,
    W::Writer: std::io::Write,
{
    materialize_view(
        txn,
        changes,
        working_copy,
        MaterializeOptions::new().prefix(prefix),
    )
}
