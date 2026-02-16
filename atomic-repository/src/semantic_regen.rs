//! Semantic regeneration — rebuilds SEMANTIC sections from graph + content.
//!
//! When a thin pull downloads only GRAPH + CONTENT sections (skipping SEMANTIC),
//! or when the tokenizer is upgraded and semantic sections need refreshing,
//! this module regenerates the SEMANTIC layer by tokenizing the content into
//! the Trunk → Branch → Leaf hierarchy.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │                     Semantic Regeneration Pipeline                        │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │                                                                          │
//! │  Input: Change with GRAPH sections + CONTENT chunks (no SEMANTIC)        │
//! │                                                                          │
//! │  1. Load change metadata + graph sections from redb                      │
//! │  2. Load content chunks and reconstruct full content blob                │
//! │  3. For each file in the graph sections:                                 │
//! │     a. Extract the file path and content range                           │
//! │     b. Detect encoding (UTF-8, binary, etc.)                             │
//! │     c. Tokenize content into lines and tokens (CrdtChangeBuilder)        │
//! │     d. Build FileOps (Trunk → Branch → Leaf)                             │
//! │  4. Store regenerated SEMANTIC sections in CHANGE_SEMANTIC table          │
//! │                                                                          │
//! │  Steps 3a-3d run in parallel across files via rayon.                     │
//! │                                                                          │
//! │  Output: CHANGE_SEMANTIC entries in redb for each file                   │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage Patterns
//!
//! ## After thin pull (eager background regeneration)
//!
//! ```rust,ignore
//! use atomic_repository::semantic_regen::{regenerate_change, RegenOptions};
//!
//! // After pull completes with layers=graph,content:
//! let hashes = pull_result.downloaded_change_hashes();
//! let options = RegenOptions::default();
//!
//! for hash in &hashes {
//!     if needs_semantic_regen(&redb_store, hash)? {
//!         let stats = regenerate_change(&redb_store, hash, &options)?;
//!         println!("Regenerated {} files, {} lines, {} tokens",
//!             stats.files_regenerated, stats.lines_generated, stats.tokens_generated);
//!     }
//! }
//! ```
//!
//! ## On-the-fly fallback (when semantic isn't ready yet)
//!
//! ```rust,ignore
//! use atomic_repository::semantic_regen::tokenize_content_for_display;
//!
//! // When diff/blame needs semantic data but it hasn't been regenerated yet:
//! let file_ops = tokenize_content_for_display(path, content, encoding);
//! // Use file_ops for display without persisting to redb
//! ```
//!
//! ## Batch regeneration (after tokenizer upgrade)
//!
//! ```rust,ignore
//! use atomic_repository::semantic_regen::{regenerate_all, RegenOptions};
//!
//! let options = RegenOptions::default().force(true); // regenerate even if exists
//! let stats = regenerate_all(&redb_store, &options)?;
//! println!("Regenerated {} changes", stats.changes_regenerated);
//! ```
//!
//! # Thread Safety
//!
//! All functions in this module are thread-safe. `regenerate_change` uses rayon
//! internally for per-file parallelism. Multiple changes can be regenerated
//! concurrently from different threads (redb handles write serialization).

use atomic_core::change::format_v3::GraphSectionPayload;
use atomic_core::change::ops::FileOps;
use atomic_core::change::Encoding;
use atomic_core::crdt::TrunkId;
use atomic_core::record::workflow::crdt::{CrdtBuildStats, CrdtChangeBuilder};
use atomic_core::types::NodeId;
use rayon::prelude::*;
use std::fmt;
use std::time::Instant;
use thiserror::Error;

use crate::redb_change_store::{RedbChangeStore, RedbStoreError};

// ═══════════════════════════════════════════════════════════════════════
// Error types
// ═══════════════════════════════════════════════════════════════════════

/// Errors from semantic regeneration.
#[derive(Debug, Error)]
pub enum RegenError {
    /// Error accessing the redb store.
    #[error("Store error: {0}")]
    Store(#[from] RedbStoreError),

    /// Error during tokenization.
    #[error("Tokenization error for {path}: {reason}")]
    Tokenize {
        /// File path that failed.
        path: String,
        /// What went wrong.
        reason: String,
    },

    /// Error deserializing a graph section payload.
    #[error("Graph section error: {0}")]
    GraphSection(String),

    /// The change doesn't exist in the store.
    #[error("Change not found: {hash}")]
    NotFound {
        /// Hex-encoded hash prefix.
        hash: String,
    },

    /// Postcard deserialization error.
    #[error("Serialization error: {0}")]
    Serialization(String),
}

/// Convenience result type.
pub type RegenResult<T> = Result<T, RegenError>;

// ═══════════════════════════════════════════════════════════════════════
// RegenOptions — configuration
// ═══════════════════════════════════════════════════════════════════════

/// Options for semantic regeneration.
///
/// Controls whether to regenerate existing sections, parallelism threshold,
/// and which files to include/exclude.
///
/// # Examples
///
/// ```rust
/// use atomic_repository::semantic_regen::RegenOptions;
///
/// // Default: skip files that already have semantic sections
/// let opts = RegenOptions::default();
/// assert!(!opts.force);
///
/// // Force regeneration (e.g., after tokenizer upgrade)
/// let opts = RegenOptions::default().force(true);
/// assert!(opts.force);
/// ```
#[derive(Clone, Debug)]
pub struct RegenOptions {
    /// If `true`, regenerate semantic sections even if they already exist.
    /// Useful after a tokenizer upgrade when all semantic data should be refreshed.
    ///
    /// Default: `false`.
    pub force: bool,

    /// Minimum number of files before using rayon parallel processing.
    /// Below this threshold, files are processed sequentially.
    ///
    /// Default: `4`.
    pub parallel_threshold: usize,

    /// Skip binary files during regeneration.
    ///
    /// Default: `true`.
    pub skip_binary: bool,
}

impl RegenOptions {
    /// Set the force flag (regenerate even if semantic sections exist).
    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Set the parallel threshold.
    pub fn parallel_threshold(mut self, threshold: usize) -> Self {
        self.parallel_threshold = threshold;
        self
    }

    /// Set whether to skip binary files.
    pub fn skip_binary(mut self, skip: bool) -> Self {
        self.skip_binary = skip;
        self
    }
}

impl Default for RegenOptions {
    fn default() -> Self {
        Self {
            force: false,
            parallel_threshold: 4,
            skip_binary: true,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RegenStats — statistics
// ═══════════════════════════════════════════════════════════════════════

/// Statistics from a single change regeneration.
#[derive(Clone, Debug, Default)]
pub struct RegenStats {
    /// Number of files for which semantic sections were generated.
    pub files_regenerated: usize,

    /// Number of files skipped (binary, empty, already had semantic).
    pub files_skipped: usize,

    /// Number of files that had errors during tokenization.
    pub files_errored: usize,

    /// Total lines generated across all files.
    pub lines_generated: usize,

    /// Total tokens generated across all files.
    pub tokens_generated: usize,

    /// Wall-clock time for the regeneration in milliseconds.
    pub elapsed_ms: u64,

    /// Whether parallel processing was used.
    pub used_parallel: bool,
}

impl fmt::Display for RegenStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} files regenerated ({} skipped, {} errors), {} lines, {} tokens in {}ms",
            self.files_regenerated,
            self.files_skipped,
            self.files_errored,
            self.lines_generated,
            self.tokens_generated,
            self.elapsed_ms,
        )?;
        if self.used_parallel {
            write!(f, " [parallel]")?;
        }
        Ok(())
    }
}

/// Statistics from a batch regeneration of multiple changes.
#[derive(Clone, Debug, Default)]
pub struct BatchRegenStats {
    /// Number of changes processed.
    pub changes_processed: usize,

    /// Number of changes that had semantic sections regenerated.
    pub changes_regenerated: usize,

    /// Number of changes skipped (already had semantic, or not found).
    pub changes_skipped: usize,

    /// Number of changes that had errors.
    pub changes_errored: usize,

    /// Aggregate file/line/token counts.
    pub total_files: usize,
    /// Aggregate lines generated.
    pub total_lines: usize,
    /// Aggregate tokens generated.
    pub total_tokens: usize,

    /// Wall-clock time for the entire batch in milliseconds.
    pub elapsed_ms: u64,
}

impl fmt::Display for BatchRegenStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} changes regenerated ({} skipped, {} errors), {} files, {} lines, {} tokens in {}ms",
            self.changes_regenerated,
            self.changes_skipped,
            self.changes_errored,
            self.total_files,
            self.total_lines,
            self.total_tokens,
            self.elapsed_ms,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Per-file regeneration result (internal, for rayon)
// ═══════════════════════════════════════════════════════════════════════

/// Result of tokenizing a single file's content.
struct FileRegenResult {
    /// The file path.
    path: String,
    /// The generated FileOps (Trunk → Branch → Leaf).
    file_ops: FileOps,
    /// Tokenization statistics.
    crdt_stats: CrdtBuildStats,
    /// Whether the file was skipped (binary, empty).
    skipped: bool,
    /// Error message if tokenization failed.
    error: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════
// needs_semantic_regen — check if a change needs regeneration
// ═══════════════════════════════════════════════════════════════════════

/// Check whether a change needs semantic regeneration.
///
/// Returns `true` if the change has GRAPH sections but no SEMANTIC sections
/// in the redb store. Returns `false` if semantic sections already exist
/// (unless `force` is true in the options).
///
/// # Arguments
///
/// * `store` - The redb change store.
/// * `hash` - The change's content hash.
///
/// # Errors
///
/// Returns an error if the change doesn't exist or the store can't be read.
///
/// # Examples
///
/// ```rust,ignore
/// if needs_semantic_regen(&store, &hash)? {
///     regenerate_change(&store, &hash, &RegenOptions::default())?;
/// }
/// ```
pub fn needs_semantic_regen(store: &RedbChangeStore, hash: &[u8; 32]) -> RegenResult<bool> {
    let meta = store.load_meta(hash)?;

    // No graph sections means nothing to regenerate from
    if meta.graph_section_count == 0 {
        return Ok(false);
    }

    // Has semantic sections already
    if meta.semantic_section_count > 0 {
        return Ok(false);
    }

    Ok(true)
}

// ═══════════════════════════════════════════════════════════════════════
// tokenize_content_for_display — on-the-fly fallback (no persistence)
// ═══════════════════════════════════════════════════════════════════════

/// Tokenize file content into FileOps for display purposes.
///
/// This is the **on-the-fly fallback** — when `diff`/`log`/`blame` needs
/// semantic data but the SEMANTIC section hasn't been regenerated yet,
/// this function tokenizes the content directly without persisting to redb.
///
/// This is the same tokenization pipeline used during `record`, but
/// standalone and stateless.
///
/// # Arguments
///
/// * `path` - File path (for the TrunkOp and display).
/// * `content` - The file content bytes.
/// * `encoding` - The detected encoding (UTF-8, binary, etc.).
///
/// # Returns
///
/// A `FileOps` containing the Trunk → Branch → Leaf hierarchy for this file.
///
/// # Examples
///
/// ```rust
/// use atomic_repository::semantic_regen::tokenize_content_for_display;
/// use atomic_core::change::Encoding;
///
/// let content = b"fn main() {\n    println!(\"hello\");\n}\n";
/// let ops = tokenize_content_for_display("src/main.rs", content, Encoding::Utf8);
///
/// assert_eq!(ops.path(), "src/main.rs");
/// assert!(ops.is_create());
/// assert!(ops.line_count() > 0);
/// ```
pub fn tokenize_content_for_display(path: &str, content: &[u8], encoding: Encoding) -> FileOps {
    let placeholder_change_id = NodeId::new(0);
    let mut builder = CrdtChangeBuilder::new(placeholder_change_id);

    let enc = if encoding == Encoding::Binary {
        None
    } else {
        Some(encoding)
    };

    builder.add_file_with_content(path, content, enc);

    let result = builder.finish();
    let (file_ops_list, _, _) = result.into_parts();

    // Extract the single FileOps for this file
    file_ops_list
        .into_iter()
        .next()
        .map(|builder_ops| builder_ops.into_change_ops())
        .unwrap_or_else(|| {
            FileOps::create(
                TrunkId::new(placeholder_change_id, 0),
                path.to_string(),
                Some(encoding),
            )
        })
}

/// Detect the encoding of file content.
///
/// Simple heuristic: if the content contains null bytes, it's binary.
/// Otherwise, check if it's valid UTF-8.
fn detect_encoding(content: &[u8]) -> Encoding {
    if content.contains(&0) {
        Encoding::Binary
    } else if std::str::from_utf8(content).is_ok() {
        Encoding::Utf8
    } else {
        Encoding::Binary
    }
}

// ═══════════════════════════════════════════════════════════════════════
// regenerate_change — the main regeneration function
// ═══════════════════════════════════════════════════════════════════════

/// Regenerate SEMANTIC sections for a single change from its GRAPH + CONTENT.
///
/// This is the core regeneration function. It:
/// 1. Loads the GRAPH sections to find file paths and content ranges
/// 2. Loads and reassembles the full content blob from CONTENT_CHUNKS
/// 3. For each file, tokenizes the content into Trunk → Branch → Leaf ops
/// 4. Stores the generated SEMANTIC sections in `CHANGE_SEMANTIC`
///
/// Files are tokenized in parallel using rayon when there are enough of them
/// (controlled by `options.parallel_threshold`).
///
/// # Arguments
///
/// * `store` - The redb change store (must have GRAPH + CONTENT for this change).
/// * `hash` - The change's content hash.
/// * `options` - Regeneration options.
///
/// # Returns
///
/// Statistics about the regeneration.
///
/// # Errors
///
/// Returns an error if the change doesn't exist, or if tokenization
/// fails for all files.
///
/// # Examples
///
/// ```rust,ignore
/// let stats = regenerate_change(&store, &hash, &RegenOptions::default())?;
/// println!("{}", stats);
/// ```
pub fn regenerate_change(
    store: &RedbChangeStore,
    hash: &[u8; 32],
    options: &RegenOptions,
) -> RegenResult<RegenStats> {
    let start = Instant::now();

    // Check if regeneration is needed
    if !options.force && !needs_semantic_regen(store, hash)? {
        return Ok(RegenStats {
            files_skipped: 1,
            ..Default::default()
        });
    }

    // Load graph sections to find file paths and content ranges
    let graph_sections = store.load_graph_sections(hash)?;
    if graph_sections.is_empty() {
        return Ok(RegenStats::default());
    }

    // Load the full content blob
    let content = store.load_full_content(hash)?;

    // Parse graph sections to extract file info
    let mut file_inputs: Vec<(String, Vec<u8>)> = Vec::new();

    for section in &graph_sections {
        // Try to parse as GraphSectionPayload to get the path and content range
        match GraphSectionPayload::from_postcard_bytes(&section.payload) {
            Ok(payload) => {
                let path = payload.path().to_string();
                let content_start = payload.content_start as usize;
                let content_end = payload.content_end as usize;

                // Extract this file's content from the blob
                let file_content = if content_start < content_end && content_end <= content.len() {
                    content[content_start..content_end].to_vec()
                } else if !content.is_empty() && path.is_empty() {
                    // Single graph section with no path = all content belongs to it
                    content.clone()
                } else {
                    Vec::new()
                };

                if !path.is_empty() {
                    file_inputs.push((path, file_content));
                }
            }
            Err(e) => {
                // If the graph section can't be parsed (e.g., it's the old
                // all-in-one format), try to use the full content directly.
                // This handles the transition period where graph sections
                // don't have per-file granularity yet.
                log::debug!(
                    "Could not parse graph section as GraphSectionPayload: {}",
                    e
                );
            }
        }
    }

    // If we didn't get any file inputs from graph sections but have content,
    // create a single unnamed entry (best-effort for non-per-file graph sections)
    if file_inputs.is_empty() && !content.is_empty() {
        file_inputs.push(("(unknown)".to_string(), content));
    }

    if file_inputs.is_empty() {
        return Ok(RegenStats::default());
    }

    // Tokenize each file's content — in parallel if above threshold
    let use_parallel = file_inputs.len() >= options.parallel_threshold;
    let skip_binary = options.skip_binary;

    let results: Vec<FileRegenResult> = if use_parallel {
        file_inputs
            .par_iter()
            .map(|(path, content)| tokenize_single_file(path, content, skip_binary))
            .collect()
    } else {
        file_inputs
            .iter()
            .map(|(path, content)| tokenize_single_file(path, content, skip_binary))
            .collect()
    };

    // Collect results and build semantic section payloads
    let mut stats = RegenStats {
        used_parallel: use_parallel,
        ..Default::default()
    };

    let mut semantic_payloads: Vec<Vec<u8>> = Vec::new();

    for result in &results {
        if let Some(ref error) = result.error {
            log::warn!("Semantic regen error for {}: {}", result.path, error);
            stats.files_errored += 1;
            continue;
        }

        if result.skipped {
            stats.files_skipped += 1;
            continue;
        }

        stats.files_regenerated += 1;
        stats.lines_generated += result.crdt_stats.lines_added;
        stats.tokens_generated += result.crdt_stats.tokens_added;

        // Serialize the FileOps for storage
        // We store each file's FileOps as a single semantic section,
        // matching the format that Change::serialize produces.
        match postcard::to_allocvec(&vec![result.file_ops.clone()]) {
            Ok(bytes) => {
                semantic_payloads.push(bytes);
            }
            Err(e) => {
                log::warn!("Failed to serialize FileOps for {}: {}", result.path, e);
                stats.files_errored += 1;
            }
        }
    }

    // Store the regenerated semantic sections in redb.
    // We write all semantic sections for this change in a single batch.
    if !semantic_payloads.is_empty() {
        store_semantic_sections(store, hash, &semantic_payloads)?;
    }

    stats.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(stats)
}

/// Tokenize a single file's content.
///
/// This runs on a rayon worker thread for parallel regeneration.
fn tokenize_single_file(path: &str, content: &[u8], skip_binary: bool) -> FileRegenResult {
    if content.is_empty() {
        return FileRegenResult {
            path: path.to_string(),
            file_ops: FileOps::create(TrunkId::new(NodeId::new(0), 0), path.to_string(), None),
            crdt_stats: CrdtBuildStats::default(),
            skipped: true,
            error: None,
        };
    }

    let encoding = detect_encoding(content);

    if encoding == Encoding::Binary && skip_binary {
        return FileRegenResult {
            path: path.to_string(),
            file_ops: FileOps::create(
                TrunkId::new(NodeId::new(0), 0),
                path.to_string(),
                Some(Encoding::Binary),
            ),
            crdt_stats: CrdtBuildStats::default(),
            skipped: true,
            error: None,
        };
    }

    // Tokenize using the same pipeline as record
    let placeholder_change_id = NodeId::new(0);
    let mut builder = CrdtChangeBuilder::new(placeholder_change_id);

    let enc = if encoding == Encoding::Binary {
        None
    } else {
        Some(encoding)
    };

    builder.add_file_with_content(path, content, enc);

    let result = builder.finish();
    let crdt_stats = result.stats().clone();
    let (file_ops_list, _, _) = result.into_parts();

    let file_ops = file_ops_list
        .into_iter()
        .next()
        .map(|builder_ops| builder_ops.into_change_ops())
        .unwrap_or_else(|| {
            FileOps::create(
                TrunkId::new(placeholder_change_id, 0),
                path.to_string(),
                enc,
            )
        });

    FileRegenResult {
        path: path.to_string(),
        file_ops,
        crdt_stats,
        skipped: false,
        error: None,
    }
}

/// Store regenerated semantic sections in the redb CHANGE_SEMANTIC table.
///
/// This updates the change's metadata to reflect the new semantic section count.
fn store_semantic_sections(
    store: &RedbChangeStore,
    hash: &[u8; 32],
    payloads: &[Vec<u8>],
) -> RegenResult<()> {
    // We need direct redb access to write semantic sections without
    // going through the full import pipeline. Access the database
    // through the store's public API by doing a save-roundtrip
    // of the metadata with updated counts.
    //
    // For now, use the import path: export the change to V3 bytes,
    // modify it to include semantic sections, and re-import.
    // This is not optimal but correct and avoids exposing redb internals.
    //
    // TODO: Add a dedicated `store_semantic_sections` method to RedbChangeStore
    // that writes directly to CHANGE_SEMANTIC without a full re-import.

    // Load the current change
    let change = store.load_change(hash).map_err(|e| RegenError::Store(e))?;

    // Build a new change with the regenerated semantic ops
    // The payloads are serialized Vec<FileOps>, but we need to extract them
    let mut all_file_ops: Vec<FileOps> = Vec::new();
    for payload in payloads {
        match postcard::from_bytes::<Vec<FileOps>>(payload) {
            Ok(ops) => {
                all_file_ops.extend(ops);
            }
            Err(e) => {
                log::warn!("Failed to deserialize FileOps payload: {}", e);
            }
        }
    }

    // Create a new Change with the semantic ops included
    let mut updated = change;
    updated.hashed.file_ops = all_file_ops;

    // Re-save to the store (this will update both graph and semantic sections)
    store.save_change(&updated).map_err(RegenError::Store)?;

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// regenerate_all — batch regeneration
// ═══════════════════════════════════════════════════════════════════════

/// Regenerate SEMANTIC sections for all changes that need it.
///
/// Scans the redb store for changes that have GRAPH sections but no
/// SEMANTIC sections, and regenerates them. If `options.force` is true,
/// regenerates all changes regardless.
///
/// # Arguments
///
/// * `store` - The redb change store.
/// * `change_hashes` - List of change hashes to consider.
/// * `options` - Regeneration options.
///
/// # Returns
///
/// Batch statistics.
///
/// # Examples
///
/// ```rust,ignore
/// let hashes = get_all_change_hashes(&store)?;
/// let stats = regenerate_batch(&store, &hashes, &RegenOptions::default())?;
/// println!("{}", stats);
/// ```
pub fn regenerate_batch(
    store: &RedbChangeStore,
    change_hashes: &[[u8; 32]],
    options: &RegenOptions,
) -> RegenResult<BatchRegenStats> {
    let start = Instant::now();
    let mut batch_stats = BatchRegenStats::default();

    for hash in change_hashes {
        batch_stats.changes_processed += 1;

        match regenerate_change(store, hash, options) {
            Ok(stats) => {
                if stats.files_regenerated > 0 {
                    batch_stats.changes_regenerated += 1;
                    batch_stats.total_files += stats.files_regenerated;
                    batch_stats.total_lines += stats.lines_generated;
                    batch_stats.total_tokens += stats.tokens_generated;
                } else {
                    batch_stats.changes_skipped += 1;
                }
            }
            Err(e) => {
                log::warn!("Failed to regenerate change: {}", e);
                batch_stats.changes_errored += 1;
            }
        }
    }

    batch_stats.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(batch_stats)
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Change, ChangeHeader, Encoding};

    // ── tokenize_content_for_display ───────────────────────────────

    #[test]
    fn test_tokenize_simple_content() {
        let content = b"fn main() {\n    println!(\"hello\");\n}\n";
        let ops = tokenize_content_for_display("src/main.rs", content, Encoding::Utf8);

        assert_eq!(ops.path(), "src/main.rs");
        assert!(ops.is_create());
        assert!(ops.line_count() > 0);
        assert!(ops.token_count() > 0);
    }

    #[test]
    fn test_tokenize_empty_content() {
        let ops = tokenize_content_for_display("empty.txt", b"", Encoding::Utf8);
        assert_eq!(ops.path(), "empty.txt");
    }

    #[test]
    fn test_tokenize_single_line() {
        let content = b"hello world\n";
        let ops = tokenize_content_for_display("hello.txt", content, Encoding::Utf8);

        assert_eq!(ops.path(), "hello.txt");
        assert!(ops.line_count() >= 1);
    }

    #[test]
    fn test_tokenize_multi_line() {
        let content = b"line 1\nline 2\nline 3\n";
        let ops = tokenize_content_for_display("multi.txt", content, Encoding::Utf8);

        assert!(ops.line_count() >= 3);
    }

    #[test]
    fn test_tokenize_binary_content() {
        let content = &[0x00, 0x01, 0x02, 0xFF, 0xFE];
        let ops = tokenize_content_for_display("binary.dat", content, Encoding::Binary);

        assert_eq!(ops.path(), "binary.dat");
    }

    #[test]
    fn test_tokenize_rust_source() {
        let content = br#"use std::io;

/// A greeting function.
fn greet(name: &str) -> String {
    format!("Hello, {}!", name)
}

fn main() {
    let name = "World";
    println!("{}", greet(name));
}
"#;
        let ops = tokenize_content_for_display("src/main.rs", content, Encoding::Utf8);

        assert!(ops.line_count() >= 10);
        assert!(ops.token_count() > ops.line_count()); // more tokens than lines
    }

    // ── detect_encoding ────────────────────────────────────────────

    #[test]
    fn test_detect_encoding_utf8() {
        assert_eq!(detect_encoding(b"hello world"), Encoding::Utf8);
        assert_eq!(detect_encoding(b"fn main() {}"), Encoding::Utf8);
        assert_eq!(detect_encoding("日本語".as_bytes()), Encoding::Utf8);
    }

    #[test]
    fn test_detect_encoding_binary() {
        assert_eq!(detect_encoding(&[0x00, 0x01, 0x02]), Encoding::Binary);
        assert_eq!(detect_encoding(&[0xFF, 0x00, 0xFE]), Encoding::Binary);
    }

    #[test]
    fn test_detect_encoding_empty() {
        assert_eq!(detect_encoding(b""), Encoding::Utf8);
    }

    // ── RegenOptions ───────────────────────────────────────────────

    #[test]
    fn test_options_default() {
        let opts = RegenOptions::default();
        assert!(!opts.force);
        assert_eq!(opts.parallel_threshold, 4);
        assert!(opts.skip_binary);
    }

    #[test]
    fn test_options_force() {
        let opts = RegenOptions::default().force(true);
        assert!(opts.force);
    }

    #[test]
    fn test_options_parallel_threshold() {
        let opts = RegenOptions::default().parallel_threshold(10);
        assert_eq!(opts.parallel_threshold, 10);
    }

    #[test]
    fn test_options_skip_binary() {
        let opts = RegenOptions::default().skip_binary(false);
        assert!(!opts.skip_binary);
    }

    // ── RegenStats ─────────────────────────────────────────────────

    #[test]
    fn test_stats_default() {
        let stats = RegenStats::default();
        assert_eq!(stats.files_regenerated, 0);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.files_errored, 0);
        assert_eq!(stats.lines_generated, 0);
        assert_eq!(stats.tokens_generated, 0);
        assert!(!stats.used_parallel);
    }

    #[test]
    fn test_stats_display() {
        let stats = RegenStats {
            files_regenerated: 10,
            files_skipped: 2,
            files_errored: 1,
            lines_generated: 500,
            tokens_generated: 3000,
            elapsed_ms: 150,
            used_parallel: true,
        };
        let display = format!("{}", stats);
        assert!(display.contains("10 files"));
        assert!(display.contains("500 lines"));
        assert!(display.contains("3000 tokens"));
        assert!(display.contains("parallel"));
    }

    #[test]
    fn test_stats_display_sequential() {
        let stats = RegenStats {
            files_regenerated: 1,
            elapsed_ms: 10,
            ..Default::default()
        };
        let display = format!("{}", stats);
        assert!(!display.contains("parallel"));
    }

    // ── BatchRegenStats ────────────────────────────────────────────

    #[test]
    fn test_batch_stats_default() {
        let stats = BatchRegenStats::default();
        assert_eq!(stats.changes_processed, 0);
        assert_eq!(stats.changes_regenerated, 0);
    }

    #[test]
    fn test_batch_stats_display() {
        let stats = BatchRegenStats {
            changes_processed: 10,
            changes_regenerated: 8,
            changes_skipped: 1,
            changes_errored: 1,
            total_files: 50,
            total_lines: 5000,
            total_tokens: 30000,
            elapsed_ms: 1500,
        };
        let display = format!("{}", stats);
        assert!(display.contains("8 changes"));
        assert!(display.contains("50 files"));
        assert!(display.contains("1500ms"));
    }

    // ── tokenize_single_file (internal) ────────────────────────────

    #[test]
    fn test_tokenize_single_file_normal() {
        let result = tokenize_single_file("test.rs", b"fn main() {}\n", false);
        assert!(!result.skipped);
        assert!(result.error.is_none());
        assert_eq!(result.path, "test.rs");
        assert!(result.crdt_stats.lines_added >= 1);
        assert!(result.crdt_stats.tokens_added >= 1);
    }

    #[test]
    fn test_tokenize_single_file_empty() {
        let result = tokenize_single_file("empty.txt", b"", false);
        assert!(result.skipped);
    }

    #[test]
    fn test_tokenize_single_file_binary_skipped() {
        let binary = &[0x00, 0x01, 0xFF];
        let result = tokenize_single_file("data.bin", binary, true);
        assert!(result.skipped);
    }

    #[test]
    fn test_tokenize_single_file_binary_not_skipped() {
        let binary = &[0x00, 0x01, 0xFF];
        let result = tokenize_single_file("data.bin", binary, false);
        assert!(!result.skipped);
    }

    // ── needs_semantic_regen (with redb store) ─────────────────────

    #[test]
    fn test_needs_regen_with_store() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("regen_test.redb");
        let store = RedbChangeStore::open(&db_path).unwrap();

        // Save a change (it will have both graph and semantic)
        let change = Change::new(
            ChangeHeader::new("test"),
            vec![],
            b"content".to_vec(),
            vec![],
        );
        let hash = store.save_change(&change).unwrap();

        // A fully-saved change should NOT need regen
        // (save_change serializes with semantic sections included)
        let needs = needs_semantic_regen(&store, &hash).unwrap();
        // This may or may not be true depending on whether the change
        // has hunks that produce semantic sections. A change with no hunks
        // has graph_section_count=0, so needs_regen returns false.
        assert!(!needs); // no graph sections = nothing to regen from
    }

    #[test]
    fn test_needs_regen_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("regen_notfound.redb");
        let store = RedbChangeStore::open(&db_path).unwrap();

        let bogus = [0xFF; 32];
        let result = needs_semantic_regen(&store, &bogus);
        assert!(result.is_err());
    }

    // ── regenerate_change (with redb store) ────────────────────────

    #[test]
    fn test_regenerate_change_no_graph() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("regen_nograph.redb");
        let store = RedbChangeStore::open(&db_path).unwrap();

        // Save a change with no hunks (no graph sections)
        let change = Change::new(
            ChangeHeader::new("empty"),
            vec![],
            b"content".to_vec(),
            vec![],
        );
        let hash = store.save_change(&change).unwrap();

        let stats = regenerate_change(&store, &hash, &RegenOptions::default()).unwrap();
        // No graph sections → nothing to regenerate
        assert_eq!(stats.files_regenerated, 0);
    }

    #[test]
    fn test_regenerate_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("regen_missing.redb");
        let store = RedbChangeStore::open(&db_path).unwrap();

        let bogus = [0xAA; 32];
        let result = regenerate_change(&store, &bogus, &RegenOptions::default());
        assert!(result.is_err());
    }

    // ── regenerate_batch ───────────────────────────────────────────

    #[test]
    fn test_regenerate_batch_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("batch_empty.redb");
        let store = RedbChangeStore::open(&db_path).unwrap();

        let stats = regenerate_batch(&store, &[], &RegenOptions::default()).unwrap();
        assert_eq!(stats.changes_processed, 0);
        assert_eq!(stats.changes_regenerated, 0);
    }

    #[test]
    fn test_regenerate_batch_with_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("batch_changes.redb");
        let store = RedbChangeStore::open(&db_path).unwrap();

        // Save some changes
        let mut hashes = Vec::new();
        for i in 0..3 {
            let change = Change::new(
                ChangeHeader::new(format!("change {}", i)),
                vec![],
                format!("content {}", i).as_bytes().to_vec(),
                vec![],
            );
            let hash = store.save_change(&change).unwrap();
            hashes.push(hash);
        }

        let stats = regenerate_batch(&store, &hashes, &RegenOptions::default()).unwrap();
        assert_eq!(stats.changes_processed, 3);
        // All 3 should be skipped (no graph sections in these simple changes)
        assert_eq!(stats.changes_skipped, 3);
    }

    #[test]
    fn test_regenerate_batch_handles_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("batch_missing.redb");
        let store = RedbChangeStore::open(&db_path).unwrap();

        let bogus = [[0xFF; 32], [0xEE; 32]];
        let stats = regenerate_batch(&store, &bogus, &RegenOptions::default()).unwrap();
        assert_eq!(stats.changes_processed, 2);
        assert_eq!(stats.changes_errored, 2);
    }

    // ── Error types ────────────────────────────────────────────────

    #[test]
    fn test_error_display() {
        let err = RegenError::Tokenize {
            path: "test.rs".into(),
            reason: "invalid utf-8".into(),
        };
        let display = format!("{}", err);
        assert!(display.contains("test.rs"));
        assert!(display.contains("invalid utf-8"));
    }

    #[test]
    fn test_error_not_found() {
        let err = RegenError::NotFound {
            hash: "AABB".into(),
        };
        let display = format!("{}", err);
        assert!(display.contains("AABB"));
    }

    #[test]
    fn test_error_graph_section() {
        let err = RegenError::GraphSection("corrupt data".into());
        assert!(format!("{}", err).contains("corrupt"));
    }

    // ── FileRegenResult (internal) ─────────────────────────────────

    #[test]
    fn test_file_regen_result_skipped() {
        let result = FileRegenResult {
            path: "test.rs".into(),
            file_ops: FileOps::create(TrunkId::new(NodeId::new(0), 0), "test.rs".into(), None),
            crdt_stats: CrdtBuildStats::default(),
            skipped: true,
            error: None,
        };
        assert!(result.skipped);
        assert!(result.error.is_none());
    }

    #[test]
    fn test_file_regen_result_with_error() {
        let result = FileRegenResult {
            path: "bad.rs".into(),
            file_ops: FileOps::create(TrunkId::new(NodeId::new(0), 0), "bad.rs".into(), None),
            crdt_stats: CrdtBuildStats::default(),
            skipped: false,
            error: Some("tokenization failed".into()),
        };
        assert!(result.error.is_some());
    }
}
