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

pub(crate) mod crdt;
mod options;
mod types;

#[cfg(test)]
mod tests;

pub use options::RecordingOptions;
pub use types::{RecordedFile, RecordingResult, RecordingStats};

use crate::change::{Encoding, FileOps, Local};
use crate::crdt::{BranchId, TrunkId};
use crate::output::WorkingCopyRead;
use crate::types::NodeId;

use super::compare::{compare_content, detect_encoding};
use super::crdt::CrdtBuildStats;
use super::detect::{DetectedFile, DetectionKind};
use super::graph_op::{BuiltHunk, BuiltHunkKind, HunkBuilder};

/// A single line from a git diff, captured during Phase 1 of git import.
///
/// This is a plain data carrier, independent of any git library.
/// Origin values:
///   `+`  — line was added in the new file
///   `-`  — line was deleted from the old file
///   ` `  — context line (unchanged)
///
/// Used by [`build_crdt_ops_from_git_diff`] to build BranchOps that
/// exactly match `git diff` output.
#[derive(Debug, Clone)]
pub struct GitDiffLine {
    /// `+`, `-`, or ` `
    pub origin: char,
    /// Raw line bytes (may include trailing `\n`)
    pub content: Vec<u8>,
    /// 1-based line number in the old file (`None` for added lines)
    pub old_lineno: Option<u32>,
    /// 1-based line number in the new file (`None` for deleted lines)
    pub new_lineno: Option<u32>,
}

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
/// * `old_content` - The pristine byte-graph (old) content
/// * `crdt_old_content` - Optional CRDT-materialized old content for CRDT op generation
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
/// let recorded = record_modified_file(&working_copy, &detected_file, &old_content, &options, None)?;
/// ```
///
/// # `existing_branches`
///
/// When supplied, this is the file-ordered list of CRDT `BranchId`s for
/// `old_content`'s lines (index `i` is the BranchId for the 0-indexed `i`-th
/// line of the old file).  This lets `Delete` and `Modify` operations
/// reference the actual existing branches instead of fresh placeholders,
/// which is required for the CRDT layer to stay coherent across commits
/// (see RCA §11.3).  Pass `None` only when the caller has no access to the
/// pristine state (e.g., tests or in-memory pipelines) — that path emits
/// placeholders and the CRDT layer is effectively insert-only.
#[allow(clippy::type_complexity)]
pub fn record_modified_file<W>(
    working_copy: &W,
    detected: &DetectedFile,
    old_content: &[u8],
    crdt_old_content: Option<&[u8]>,
    options: &RecordingOptions,
    existing_branches: Option<&[BranchId]>,
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

    // Handle binary files: compare_content returns is_binary=true with zero
    // diff_ops when either old or new content contains null bytes.  Since we
    // know the content differs (identical content returns early above), create
    // a single Replace hunk that swaps the entire file content.  Without this,
    // binary modifications are silently dropped (zero hunks → empty RecordedFile
    // → the graph never receives the new content, and `atomic status` reports
    // the file as modified forever).
    if comparison.is_binary && old_content != &new_content[..] {
        let mut replace_hunk = BuiltHunk::new_replace_with_lines(
            Local::new(&detected.path, 1),
            Some(encoding),
            Vec::new(), // no per-line deletion tracking for binary
            0,          // old_start
            0,          // new_start
            1,          // new_len: treat entire binary blob as one unit
        );
        replace_hunk.content_start = Some(0);
        replace_hunk.content_end = Some(new_content.len() as u64);
        recorded.add_hunk(replace_hunk);
        recorded.set_content(new_content);

        return Ok(recorded);
    }

    // Build hunks from diff ops using HunkBuilder
    let hunk_options = options.to_hunk_options().encoding(encoding);
    let mut builder = HunkBuilder::with_options(&detected.path, hunk_options);

    for op in &comparison.diff_ops {
        builder.process_diff_op(op);
    }

    let hunk_result = builder.finish();

    // Calculate line offsets in the new content for mapping line numbers to byte positions
    let new_line_offsets = calculate_line_offsets(&new_content);

    // Each hunk is now processed independently by the globalize layer
    // using proper patch theory: targeted per-line deletions and
    // insertions on the specific vertices involved.  No whole-file
    // consolidation is needed — see `globalize_replace` and
    // `globalize_delete` for the line-level vertex operations.
    let hunks: Vec<BuiltHunk> = hunk_result.into_hunks();

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

    // Generate CRDT operations for token-level diff tracking.
    //
    // Load-bearing subtlety: CRDT cleanup must diff against the prior
    // CRDT materialization when available, not only the byte-graph read.
    // If the CRDT layer has already drifted from the byte graph, using the
    // byte-graph content here leaves CRDT-only stale branches alive forever
    // because the diff never "sees" them to delete them.
    //
    // Graph hunks above still use `old_content` (the byte-graph source of
    // truth for patch theory).  Only the CRDT op builder uses the optional
    // `crdt_old_content`.
    use crate::record::workflow::recipes::{Recipe, RecipeContext};
    let recipe_ctx = RecipeContext {
        path: &detected.path,
        old_content: crdt_old_content.unwrap_or(old_content),
        new_content: &new_content,
        existing_branches,
        encoding,
        algorithm: options.get_algorithm(),
    };
    let crdt_ops = Recipe::detect(&recipe_ctx).build_ops(&recipe_ctx);
    recorded.set_crdt_ops(crdt_ops.0);
    recorded.set_crdt_stats(crdt_ops.1);

    // Store the new content
    recorded.set_content(new_content);

    Ok(recorded)
}

/// Build CRDT FileOps directly from git diff lines.
///
/// This is the authoritative path for git import: instead of re-running
/// our Myers diff algorithm (which may produce different edit operations
/// than git), we translate git's own `+`/`-`/` ` line classifications
/// directly into BranchOps.
///
/// The result has exactly the same Insert/Delete operations that
/// `git diff` would show — so `atomic diff -c` output matches `git diff`
/// line-for-line.
///
/// # Arguments
///
/// * `path`       — file path (for the FileOps container)
/// * `diff_lines` — ordered slice of GitDiffLine from git2::Diff::foreach
///
/// # Returns
///
/// `(FileOps, CrdtBuildStats)` ready to be stored in a `RecordedFile`.
pub fn build_crdt_ops_from_git_diff(
    path: &str,
    diff_lines: &[GitDiffLine],
) -> (FileOps, CrdtBuildStats) {
    use super::crdt::FileOps as BuilderFileOps;
    use super::crdt::LineOps as BuilderLineOps;
    use crate::crdt::LeafOp;

    let placeholder_change_id = NodeId::new(0);
    let trunk_id = TrunkId::new(placeholder_change_id, 0);
    let mut file_ops = BuilderFileOps::new(trunk_id, path.to_string(), None);

    let mut stats = CrdtBuildStats::new();
    let mut next_branch_idx: u32 = 0;
    let mut next_leaf_idx: u32 = 0;

    let mut alloc_branch = || {
        let id = BranchId::new(placeholder_change_id, next_branch_idx);
        next_branch_idx += 1;
        id
    };

    let mut alloc_leaf = || {
        let id = crate::crdt::LeafId::new(placeholder_change_id, next_leaf_idx);
        next_leaf_idx += 1;
        id
    };

    let mut prev_branch: Option<BranchId> = None;

    for diff_line in diff_lines {
        match diff_line.origin {
            // ── Deleted line ─────────────────────────────────────────────
            '-' => {
                let branch_id = alloc_branch();

                // Store the deleted line content as leaf ops so
                // `atomic diff -c` can reconstruct the old line text.
                let content_bytes = &diff_line.content;
                // Strip trailing newline for storage (consistent with inserts).
                let trimmed = if content_bytes.ends_with(b"\n") {
                    &content_bytes[..content_bytes.len() - 1]
                } else {
                    content_bytes
                };

                let leaf_ops = vec![LeafOp::Insert {
                    after: None,
                    kind: crate::diff::TokenKind::Word,
                    content: trimmed.to_vec(),
                }];

                let mut line_op = BuilderLineOps::delete(branch_id, leaf_ops);
                if let Some(n) = diff_line.old_lineno {
                    line_op = line_op.with_old_line_num(n as usize);
                }
                file_ops.add_line_op(line_op);
                stats.lines_deleted += 1;
            }

            // ── Added line ───────────────────────────────────────────────
            '+' => {
                let branch_id = alloc_branch();

                let content_bytes = &diff_line.content;
                let trimmed = if content_bytes.ends_with(b"\n") {
                    &content_bytes[..content_bytes.len() - 1]
                } else {
                    content_bytes
                };

                let leaf_id = alloc_leaf();
                let leaf_ops = vec![LeafOp::Insert {
                    after: None,
                    kind: crate::diff::TokenKind::Word,
                    content: trimmed.to_vec(),
                }];
                let _ = leaf_id; // leaf_id allocated for ordering; content is in leaf_ops

                let mut line_op = BuilderLineOps::insert(branch_id, prev_branch, leaf_ops);
                if let Some(n) = diff_line.new_lineno {
                    line_op = line_op.with_new_line_num(n as usize);
                }
                file_ops.add_line_op(line_op);
                stats.lines_added += 1;
                prev_branch = Some(branch_id);
            }

            // ── Context line (unchanged) — update prev_branch ────────────
            ' ' => {
                // Context lines are not recorded as BranchOps; they only
                // advance the `prev_branch` cursor so subsequent inserts
                // are anchored after the right line.
                //
                // We allocate a phantom branch ID to maintain the ordering
                // chain.  It is never stored anywhere.
                let phantom_id = alloc_branch();
                prev_branch = Some(phantom_id);
            }

            _ => {} // Hunk headers, no-newline markers, etc. — skip.
        }
    }

    (file_ops.into_change_ops(), stats)
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
