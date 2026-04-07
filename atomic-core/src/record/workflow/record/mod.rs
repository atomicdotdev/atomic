//! Recording functions to build changes from detected files.
//!
//! This module provides the functionality to convert detected file changes
//! into recorded hunks that can be assembled into a [`Change`]. It bridges
//! the gap between detection (what changed) and the final change object
//! (the serializable representation).
//!
//! # CRDT Integration
//!
//! **By default**, all recording functions automatically generate CRDT operations
//! (Trunk → Branch → Leaf) alongside the traditional hunks. This enables:
//!
//! - **Token-level diff**: Fine-grained change tracking at the token level
//! - **Conflict-free merging**: CRDT operations can be applied in any order
//! - **Accurate blame**: Token-level attribution for code changes
//!
//! Access CRDT data via [`RecordedFile::crdt_ops()`] and [`RecordedFile::crdt_stats()`].
//!
//! # Overview
//!
//! Recording is the process of converting detected changes into hunks:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Recording Pipeline                                │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input: Detection Results                                               │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ DetectedFile { kind: Added, path: "new.rs", ... }                │  │
//! │  │ DetectedFile { kind: Modified, path: "lib.rs", diff_ops: [...] } │  │
//! │  │ DetectedFile { kind: Deleted, path: "old.rs", ... }              │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  Recording Process (Parallel Outputs)                                   │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ 1. For each detected file:                                       │  │
//! │  │    • Read working copy content (for adds/modifies)               │  │
//! │  │    • Retrieve pristine content (for modifies/deletes)            │  │
//! │  │    • Build traditional hunks (BuiltHunk)                         │  │
//! │  │    • Build CRDT operations (FileOps with TrunkOp/BranchOp/LeafOp)│  │
//! │  │ 2. Track content offsets in the change's content buffer          │  │
//! │  │ 3. Track inode updates for local application                     │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  Output: RecordingResult                                                │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ files: Vec<RecordedFile>                                         │  │
//! │  │   └─ hunks: Vec<BuiltHunk>       (traditional change hunks)      │  │
//! │  │   └─ crdt_ops: Option<FileOps>   (token-level CRDT operations)   │  │
//! │  │   └─ crdt_stats: CrdtBuildStats  (tokenization statistics)       │  │
//! │  │ stats: RecordingStats                                            │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ## Basic Recording
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::record::{record_detected_files, RecordingOptions};
//!
//! // Assuming we have detection results
//! let detected = detect_changes_simple(&wc, &tracked, &options);
//!
//! // Record all detected changes
//! let recording_options = RecordingOptions::new();
//! let result = record_detected_files(
//!     &working_copy,
//!     &changes,
//!     detected.changed_files(),
//!     &recording_options,
//! )?;
//!
//! println!("Recorded {} hunks", result.hunk_count());
//! println!("Content size: {} bytes", result.content_len());
//! ```
//!
//! ## Recording with Filtering
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::record::{record_file, RecordingOptions};
//!
//! // Record only specific files
//! let options = RecordingOptions::new().max_file_size(1024 * 1024);
//!
//! for file in detected.modified() {
//!     if should_record(&file) {
//!         let result = record_file(&working_copy, &changes, &file, &options)?;
//!         // Process result...
//!     }
//! }
//! ```
//!
//! # Recording Different Change Types
//!
//! ## Added Files
//!
//! For newly added files, we:
//! 1. Read the entire content from the working copy
//! 2. Detect the encoding (text vs binary)
//! 3. Create a `FileAdd` graph_op with the content
//!
//! ## Deleted Files
//!
//! For deleted files, we:
//! 1. Look up the file's position in the pristine graph
//! 2. Create a `FileDel` graph_op marking the edges as deleted
//!
//! ## Modified Files
//!
//! For modified files, we:
//! 1. Retrieve the pristine content
//! 2. Read the working copy content
//! 3. Generate diff operations
//! 4. Convert diffs to `Edit` or `Replacement` hunks
//!
//! ## Moved Files
//!
//! For moved/renamed files, we:
//! 1. Create a `FileMove` graph_op with old and new paths
//! 2. Handle any content changes as modifications
//!
//! # Performance Considerations
//!
//! - Content is read lazily when needed
//! - Large files can be skipped or handled specially
//! - The content buffer grows as needed (consider pre-allocation for large changes)
//!
//! # Error Handling
//!
//! Recording can fail for several reasons:
//!
//! - IO errors when reading files
//! - Encoding errors for binary files treated as text
//! - Missing files in the working copy
//!
//! Errors are collected in the result rather than failing immediately,
//! allowing partial recording to succeed.
//!
//! [`Change`]: crate::change::Change

mod crdt;
mod options;
mod types;

#[cfg(test)]
mod tests;

pub use options::RecordingOptions;
pub use types::{RecordedFile, RecordingResult, RecordingStats};

use crate::change::{Encoding, Local};
use crate::output::WorkingCopyRead;

use super::compare::{compare_content, detect_encoding};
use super::detect::{DetectedFile, DetectionKind};
use super::graph_op::{BuiltHunk, BuiltHunkKind, HunkBuilder};

// ============================================================================
// RECORDING FUNCTIONS
// ============================================================================

/// Record a single added file from the working copy.
///
/// Reads the file content and creates appropriate hunks for a new file.
///
/// # Arguments
///
/// * `working_copy` - Working copy interface
/// * `detected` - The detected file
/// * `options` - Recording options
///
/// # Returns
///
/// A `RecordedFile` with the file's hunks and content.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::record::{record_added_file, RecordingOptions};
///
/// let options = RecordingOptions::new();
/// let recorded = record_added_file(&working_copy, &detected_file, &options)?;
/// ```
pub fn record_added_file<W>(
    working_copy: &W,
    detected: &DetectedFile,
    options: &RecordingOptions,
) -> Result<RecordedFile, String>
where
    W: WorkingCopyRead,
{
    let mut recorded = RecordedFile::new(&detected.path);
    recorded.set_kind(DetectionKind::Added);

    // Read content from working copy
    let mut content = Vec::new();
    if let Err(e) = working_copy.read_file(&detected.path, &mut content) {
        return Err(format!("Failed to read file {}: {}", detected.path, e));
    }

    // Check size limits
    if options.exceeds_max_size(content.len()) {
        return Err(format!(
            "File {} exceeds maximum size ({} bytes)",
            detected.path,
            content.len()
        ));
    }

    // Skip empty files if configured
    if content.is_empty() && !options.get_record_empty_files() {
        return Err(format!("Skipping empty file {}", detected.path));
    }

    // Detect encoding
    let encoding = detect_encoding(&content);
    recorded.set_encoding(encoding);

    // Skip binary files if configured
    if encoding == Encoding::Binary && options.get_skip_binary() {
        return Err(format!("Skipping binary file {}", detected.path));
    }

    // Create an edit graph_op for the new content
    let local = Local::new(&detected.path, 1);
    let content_len = content.len() as u64;
    let graph_op = BuiltHunk::new_edit(local, Some(encoding), 0, content_len);
    recorded.add_hunk(graph_op);

    // Generate CRDT operations for token-level tracking
    // Use NodeId(0) as placeholder - will be resolved during change finalization
    let crdt_ops = crdt::build_crdt_ops_for_added_file(&detected.path, &content, encoding);
    recorded.set_crdt_ops(crdt_ops.0);
    recorded.set_crdt_stats(crdt_ops.1);

    // Store the content
    recorded.set_content(content);

    Ok(recorded)
}

/// Record a single deleted file.
///
/// Creates a deletion graph_op for a file that no longer exists in the working copy.
///
/// # Arguments
///
/// * `detected` - The detected file
/// * `options` - Recording options
///
/// # Returns
///
/// A `RecordedFile` with deletion hunks.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::record::{record_deleted_file, RecordingOptions};
///
/// let options = RecordingOptions::new();
/// let recorded = record_deleted_file(&detected_file, &options)?;
/// ```
pub fn record_deleted_file(
    detected: &DetectedFile,
    _options: &RecordingOptions,
) -> Result<RecordedFile, String> {
    let mut recorded = RecordedFile::new(&detected.path);
    recorded.set_kind(DetectionKind::Deleted);

    // Copy inode and position if available
    if let Some(inode) = detected.inode {
        recorded.set_inode(inode);
    }
    if let Some(position) = detected.position {
        recorded.set_position(position);
    }

    // Create a deletion graph_op for the file.
    // The actual line information would come from the pristine graph traversal
    // when integrated with the full repository context. The deletion graph_op
    // marks the file's content edges as deleted in the graph.
    let local = Local::new(&detected.path, 1);
    let encoding = detected.encoding;
    let graph_op = BuiltHunk::new_delete(local, encoding, vec![0], 0);
    recorded.add_hunk(graph_op);

    // Generate CRDT operations for the deletion
    let crdt_ops = crdt::build_crdt_ops_for_deleted_file(&detected.path);
    recorded.set_crdt_ops(crdt_ops.0);
    recorded.set_crdt_stats(crdt_ops.1);

    Ok(recorded)
}

/// Record a single modified file.
///
/// Compares old and new content and creates appropriate edit/replacement hunks.
///
/// # Arguments
///
/// * `working_copy` - Working copy interface
/// * `detected` - The detected file (should have diff_ops populated)
/// * `old_content` - The pristine (old) content
/// * `options` - Recording options
///
/// # Returns
///
/// A `RecordedFile` with modification hunks.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::record::{record_modified_file, RecordingOptions};
///
/// let options = RecordingOptions::new();
/// let recorded = record_modified_file(&working_copy, &detected_file, &old_content, &options)?;
/// ```
#[allow(clippy::type_complexity)]
pub fn record_modified_file<W>(
    working_copy: &W,
    detected: &DetectedFile,
    old_content: &[u8],
    options: &RecordingOptions,
) -> Result<RecordedFile, String>
where
    W: WorkingCopyRead,
{
    let mut recorded = RecordedFile::new(&detected.path);
    recorded.set_kind(DetectionKind::Modified);

    // Count lines in old content for precise insertion point detection
    let old_line_count = old_content.iter().filter(|&&b| b == b'\n').count();
    recorded.set_old_line_count(old_line_count);

    // Copy inode and position if available
    if let Some(inode) = detected.inode {
        recorded.set_inode(inode);
    }
    if let Some(position) = detected.position {
        recorded.set_position(position);
    }

    // Read new content from working copy
    let mut new_content = Vec::new();
    if let Err(e) = working_copy.read_file(&detected.path, &mut new_content) {
        return Err(format!("Failed to read file {}: {}", detected.path, e));
    }

    // Check size limits
    if options.exceeds_max_size(new_content.len()) {
        return Err(format!(
            "File {} exceeds maximum size ({} bytes)",
            detected.path,
            new_content.len()
        ));
    }

    // Detect encoding
    let encoding = detect_encoding(&new_content);
    recorded.set_encoding(encoding);

    // Skip binary files if configured
    if encoding == Encoding::Binary && options.get_skip_binary() {
        return Err(format!("Skipping binary file {}", detected.path));
    }

    // Compare content and build hunks
    let comparison = compare_content(old_content, &new_content, options.get_algorithm());

    // Build hunks from diff ops using HunkBuilder
    let hunk_options = options.to_hunk_options().encoding(encoding);
    let mut builder = HunkBuilder::with_options(&detected.path, hunk_options);

    for op in &comparison.diff_ops {
        builder.process_diff_op(op);
    }

    let hunk_result = builder.finish();

    // Calculate line offsets in the new content for mapping line numbers to byte positions
    let new_line_offsets = calculate_line_offsets(&new_content);

    // Add all built hunks to the recorded file, updating content positions
    // ── Consolidate multiple Replace hunks into a single whole-file Replace ──
    //
    // The diff may produce multiple Replace hunks when changes are in
    // non-contiguous regions of the file. In the graph layer, each Replace
    // hunk does delete-ALL-content + insert-full-content. With N Replace
    // hunks this creates N copies (duplication bug).
    //
    // Fix: merge all Replace hunks into ONE Replace that covers the entire
    // file. Insert and Delete hunks are kept as-is — they're additive
    // operations that don't conflict.
    //
    // The semantic layer (CRDT line_ops) is unaffected — it's built
    // separately from the raw diff and retains per-line granularity.
    let mut hunks: Vec<BuiltHunk> = hunk_result.into_hunks();
    let replace_count = hunks
        .iter()
        .filter(|h| h.kind == BuiltHunkKind::Replace)
        .count();

    if replace_count > 1 {
        // Merge all Replace hunks: union their deleted_lines, span the
        // full new content range (0..new_content.len()).
        let mut all_deleted: Vec<usize> = Vec::new();
        let mut min_old_start = usize::MAX;

        // Collect deleted lines from all Replace hunks
        for h in &hunks {
            if h.kind == BuiltHunkKind::Replace {
                all_deleted.extend_from_slice(&h.deleted_lines);
                if h.old_start < min_old_start {
                    min_old_start = h.old_start;
                }
            }
        }
        all_deleted.sort_unstable();
        all_deleted.dedup();

        if min_old_start == usize::MAX {
            min_old_start = 0;
        }

        // Build the merged Replace covering the entire new file
        let new_line_count = new_content.split(|&b| b == b'\n').count();
        let merged_replace = BuiltHunk::new_replace_with_lines(
            Local::new(&detected.path, (min_old_start + 1) as u64),
            Some(encoding),
            all_deleted,
            min_old_start,
            0,              // new_start: beginning of new content
            new_line_count, // new_len: all lines in the new file
        );

        // Remove all Replace hunks, keep Insert/Delete hunks
        hunks.retain(|h| h.kind != BuiltHunkKind::Replace);
        // Add the single merged Replace
        hunks.push(merged_replace);
    }

    for mut graph_op in hunks {
        // For Insert and Replace hunks, calculate actual content byte positions
        // based on which lines of new content they represent
        if graph_op.kind == BuiltHunkKind::Insert || graph_op.kind == BuiltHunkKind::Replace {
            // Use the graph_op's new_start and new_len to find the byte range in new_content.
            // new_start is 0-indexed line number, new_len is number of lines.
            if graph_op.new_len > 0 {
                // Find the start byte position (beginning of new_start line)
                let start_byte = if graph_op.new_start < new_line_offsets.len() {
                    new_line_offsets[graph_op.new_start]
                } else {
                    new_content.len()
                };

                // Find the end byte position (end of last line in range)
                let end_line = graph_op.new_start + graph_op.new_len;
                let end_byte = if end_line < new_line_offsets.len() {
                    new_line_offsets[end_line]
                } else {
                    new_content.len()
                };

                graph_op.content_start = Some(start_byte as u64);
                graph_op.content_end = Some(end_byte as u64);
            } else {
                // No new content (shouldn't happen for Insert/Replace, but handle gracefully)
                graph_op.content_start = Some(0);
                graph_op.content_end = Some(0);
            }
        }
        recorded.add_hunk(graph_op);
    }

    // Generate CRDT operations for token-level diff tracking
    let crdt_ops = crdt::build_crdt_ops_for_modified_file(
        &detected.path,
        old_content,
        &new_content,
        encoding,
        options.get_algorithm(),
    );
    recorded.set_crdt_ops(crdt_ops.0);
    recorded.set_crdt_stats(crdt_ops.1);

    // Store the new content
    recorded.set_content(new_content);

    Ok(recorded)
}

/// Calculate byte offsets for each line in content.
///
/// Returns a vector where index i contains the byte offset where line i starts.
/// Line 0 starts at offset 0.
fn calculate_line_offsets(content: &[u8]) -> Vec<usize> {
    let mut offsets = vec![0];
    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' && i + 1 < content.len() {
            offsets.push(i + 1);
        }
    }
    offsets
}
