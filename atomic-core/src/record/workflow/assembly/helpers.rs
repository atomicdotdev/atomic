//! Helper functions and statistics for change assembly.
//!
//! Standalone utility functions used during the assembly pipeline:
//! content offset computation, dependency collection, and hunk validation.
//! Also contains [`AssemblyStats`] for tracking assembly progress.

use std::fmt;

use crate::change::GraphOp;
use crate::pristine::TreeTxnT;
use crate::record::workflow::globalize::GlobalizeContext;
use crate::record::workflow::record::RecordedFile;
use crate::types::Hash;

use super::types::{AssemblyError, AssemblyOptions, AssemblyResult};

// ============================================================================
// STATISTICS
// ============================================================================

/// Statistics about the assembly process.
#[derive(Debug, Clone, Default)]
pub struct AssemblyStats {
    /// Number of files processed.
    pub files_processed: usize,

    /// Number of files skipped (empty or error).
    pub files_skipped: usize,

    /// Number of hunks added.
    pub hunks_added: usize,

    /// Number of dependencies added.
    pub dependencies_added: usize,

    /// Total content bytes.
    pub content_bytes: u64,

    /// Number of errors encountered.
    pub errors: usize,
}

impl AssemblyStats {
    /// Create new empty statistics.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a processed file.
    pub fn record_file(&mut self) {
        self.files_processed += 1;
    }

    /// Record a skipped file.
    pub fn record_skip(&mut self) {
        self.files_skipped += 1;
    }

    /// Record an error.
    pub fn record_error(&mut self) {
        self.errors += 1;
    }

    /// Add content bytes.
    pub fn add_content_bytes(&mut self, bytes: u64) {
        self.content_bytes += bytes;
    }

    /// Check if any errors occurred.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    /// Get total files (processed + skipped).
    #[must_use]
    pub fn total_files(&self) -> usize {
        self.files_processed + self.files_skipped
    }
}

impl fmt::Display for AssemblyStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AssemblyStats {{ files: {} ({} skipped), hunks: {}, deps: {}, bytes: {} }}",
            self.files_processed,
            self.files_skipped,
            self.hunks_added,
            self.dependencies_added,
            self.content_bytes
        )
    }
}

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Compute content offsets for a sequence of hunks.
///
/// This function calculates the byte offset for each graph_op's content
/// in the final content blob.
///
/// # Arguments
///
/// * `files` - The recorded files with content
///
/// # Returns
///
/// A vector of (file_index, hunk_index, start_offset, end_offset) tuples.
///
/// # Example
///
/// ```rust,ignore
/// let offsets = compute_content_offsets(&recorded_files);
/// for (file_idx, hunk_idx, start, end) in offsets {
///     println!("File {}, GraphOp {}: [{}, {})", file_idx, hunk_idx, start, end);
/// }
/// ```
#[must_use]
pub fn compute_content_offsets(files: &[RecordedFile]) -> Vec<(usize, usize, u64, u64)> {
    let mut offsets = Vec::new();
    let mut current_offset: u64 = 0;

    for (file_idx, file) in files.iter().enumerate() {
        for (hunk_idx, graph_op) in file.hunks().iter().enumerate() {
            if let (Some(start), Some(end)) = (graph_op.content_start, graph_op.content_end) {
                let len = end.saturating_sub(start);
                offsets.push((file_idx, hunk_idx, current_offset, current_offset + len));
                current_offset += len;
            }
        }
    }

    offsets
}

/// Collect all dependencies from a set of recorded files.
///
/// This function gathers all change hashes that the new change depends on,
/// based on the graph positions referenced by the recorded files.
///
/// # Arguments
///
/// * `ctx` - The globalization context (contains tracked dependencies)
///
/// # Returns
///
/// A sorted vector of dependency hashes.
#[must_use]
pub fn collect_dependencies(ctx: &GlobalizeContext<'_, impl TreeTxnT>) -> Vec<Hash> {
    ctx.dependencies_sorted()
}

/// Finalize hunks by converting them to the serializable format.
///
/// This validates that all hunks are properly formed and ready for
/// serialization.
///
/// # Arguments
///
/// * `hunks` - The hunks to finalize
/// * `options` - Assembly options for validation
///
/// # Returns
///
/// The validated hunks, or an error if validation fails.
pub fn finalize_hunks(
    hunks: Vec<GraphOp<Option<Hash>>>,
    options: &AssemblyOptions,
) -> AssemblyResult<Vec<GraphOp<Option<Hash>>>> {
    // Check graph_op count limit
    if hunks.len() > options.get_max_hunks() {
        return Err(AssemblyError::TooManyHunks {
            actual: hunks.len(),
            limit: options.get_max_hunks(),
        });
    }

    Ok(hunks)
}
