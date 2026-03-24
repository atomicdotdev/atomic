//! Recording changes from the working copy.
//!
//! This module provides the high-level interface for recording changes from
//! the working copy into serializable [`Change`] objects that can be applied
//! to other repositories.
//!
//! # Overview
//!
//! Recording is the process of:
//!
//! 1. **Detecting** modifications in the working copy (added, modified, deleted files)
//! 2. **Comparing** file contents with the pristine state
//! 3. **Building** hunks that describe the differences
//! 4. **Globalizing** local positions to graph vertices
//! 5. **Assembling** a complete change with dependencies
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Recording Pipeline                               │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Working Copy          Detection           Globalization    Change      │
//! │  ┌──────────┐         ┌──────────┐        ┌──────────┐    ┌──────────┐ │
//! │  │  Files   │  scan   │ Modified │ build  │ Graph    │asm │  Hash    │ │
//! │  │  on disk │ ──────▶ │ Tracked  │ ─────▶ │ Vertices │───▶│  Hunks   │ │
//! │  └──────────┘         └──────────┘        │ Edges    │    │  Content │ │
//! │       │                    │              └──────────┘    └──────────┘ │
//! │       │                    │                   │               │       │
//! │       ▼                    ▼                   ▼               ▼       │
//! │  ┌──────────┐         ┌──────────┐        ┌──────────┐    ┌──────────┐ │
//! │  │ Pristine │  diff   │ Built    │  deps  │ Depend-  │save│ Stored   │ │
//! │  │  State   │ ◄─────▶ │ Hunks    │ ─────▶ │ encies   │───▶│ on Disk  │ │
//! │  └──────────┘         └──────────┘        └──────────┘    └──────────┘ │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, RecordOptions};
//! use atomic_core::change::{Author, ChangeHeader};
//!
//! let repo = Repository::open(".")?;
//!
//! // Create change header
//! let header = ChangeHeader::builder()
//!     .message("Add new feature")
//!     .author(Author::new("Alice", Some("alice@example.com")))
//!     .build();
//!
//! // Record all changes
//! let result = repo.record(header, RecordOptions::default())?;
//! println!("Created change: {}", result.hash.to_base32());
//! ```
//!
//! # Partial Recording
//!
//! You can record a subset of changes by specifying paths:
//!
//! ```rust,ignore
//! let options = RecordOptions::default()
//!     .paths(vec!["src/main.rs", "src/lib.rs"]);
//!
//! let result = repo.record(header, options)?;
//! ```

use std::fmt;

use atomic_core::change::{Change, ChangeHeader, Encoding, Provenance};
use atomic_core::diff::Algorithm;
use atomic_core::record::workflow::{
    AssemblyError, AssemblyOptions, GlobalizeError, GlobalizeOptions,
    RecordingOptions as CoreRecordingOptions,
};
use atomic_core::types::{Base32, Hash, Merkle};

use thiserror::Error;

use crate::status::{FileStatus, FileStatusEntry};
use crate::RepositoryError;

// ERROR TYPES

/// Result type for record operations.
pub type RecordResult<T> = Result<T, RecordError>;

/// Errors that can occur during recording.
#[derive(Debug, Error)]
pub enum RecordError {
    /// No changes to record.
    ///
    /// The working copy matches the pristine state.
    #[error("Nothing to record: working copy is clean")]
    NothingToRecord,

    /// No files matched the specified paths.
    #[error("No files matched the specified paths")]
    NoFilesMatched,

    /// File not found in working copy.
    #[error("File not found: {path}")]
    FileNotFound {
        /// The path that was not found
        path: String,
    },

    /// File not tracked.
    #[error("File not tracked: {path}")]
    FileNotTracked {
        /// The untracked file path
        path: String,
    },

    /// Globalization failed.
    #[error("Failed to globalize changes: {0}")]
    Globalize(#[from] GlobalizeError),

    /// Assembly failed.
    #[error("Failed to assemble change: {0}")]
    Assembly(#[from] AssemblyError),

    /// IO error reading files.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Repository error.
    #[error("Repository error: {0}")]
    Repository(#[from] RepositoryError),

    /// Change store error.
    #[error("Change store error: {0}")]
    ChangeStore(String),

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// Invalid header.
    #[error("Invalid change header: {reason}")]
    InvalidHeader {
        /// Reason the header is invalid
        reason: String,
    },

    /// Conflicts detected.
    ///
    /// The working copy has unresolved conflicts.
    #[error("Unresolved conflicts in working copy")]
    UnresolvedConflicts,

    /// Binary file exceeds size limit.
    #[error("File too large: {path} ({size} bytes, limit: {limit})")]
    FileTooLarge {
        /// The file path
        path: String,
        /// Actual size in bytes
        size: u64,
        /// Maximum allowed size
        limit: u64,
    },
}

// OPTIONS

/// Options for recording changes.
///
/// Controls which files are recorded and how they are processed.
///
/// # Example
///
/// ```rust
/// use atomic_repository::record::RecordOptions;
/// use atomic_core::diff::Algorithm;
///
/// let options = RecordOptions::new()
///     .with_algorithm(Algorithm::Patience)
///     .with_all(true)
///     .message("Fix bug in parser");
///
/// assert_eq!(options.algorithm(), Algorithm::Patience);
/// assert!(options.all());
/// ```
#[derive(Debug, Clone)]
pub struct RecordOptions {
    /// Specific paths to record (empty = all modified files).
    paths: Vec<String>,

    /// Whether to record all changes (ignore paths filter).
    all: bool,

    /// Diff algorithm to use.
    algorithm: Algorithm,

    /// Default encoding for new files.
    default_encoding: Encoding,

    /// Maximum file size to diff (bytes).
    max_file_size: u64,

    /// Whether to skip binary files.
    skip_binary: bool,

    /// Whether to record empty files.
    record_empty_files: bool,

    /// Number of context lines for display.
    context_lines: usize,

    /// Stack to record to (None = current stack).
    stack: Option<String>,

    /// Change message (can also be set in header).
    message: Option<String>,

    /// Whether to apply the change after recording.
    apply_after_record: bool,

    /// Whether to save the change to the store.
    save_to_store: bool,

    /// AI provenance information for this change.
    ///
    /// When recording AI-assisted changes, this captures metadata about
    /// the AI involvement (vendor, model, tokens, cost, etc.).
    provenance: Vec<Provenance>,

    /// Opaque metadata bytes to store in `HashedChange.metadata`.
    ///
    /// These bytes become part of the change's cryptographic hash, making
    /// them tamper-evident. Used by `atomic-agent` to embed the
    /// `SessionEnvelope` (turn number, session ID, timing, files) so that
    /// session structure is part of the change's identity.
    ///
    /// Empty by default — most non-agent recordings don't need this.
    metadata_bytes: Vec<u8>,
}

impl RecordOptions {
    /// Default maximum file size (10 MB).
    pub const DEFAULT_MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

    /// Default number of context lines.
    pub const DEFAULT_CONTEXT_LINES: usize = 3;

    /// Create new options with defaults.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::record::RecordOptions;
    ///
    /// let options = RecordOptions::new();
    /// assert!(!options.all());
    /// assert!(options.get_apply_after_record());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set specific paths to record.
    ///
    /// When paths are specified, only changes in those files will be recorded.
    /// Other modified files will be ignored.
    ///
    /// # Arguments
    ///
    /// * `paths` - Paths to record (relative to repository root)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::record::RecordOptions;
    ///
    /// let options = RecordOptions::new()
    ///     .paths(vec!["src/main.rs", "src/lib.rs"]);
    /// ```
    #[must_use]
    pub fn paths(mut self, paths: Vec<impl Into<String>>) -> Self {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Add a single path to record.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to add
    #[must_use]
    pub fn add_path(mut self, path: impl Into<String>) -> Self {
        self.paths.push(path.into());
        self
    }

    /// Set whether to record all changes.
    ///
    /// When true, ignores the paths filter and records all modified files.
    ///
    /// # Arguments
    ///
    /// * `all` - Whether to record all changes
    #[must_use]
    pub fn with_all(mut self, all: bool) -> Self {
        self.all = all;
        self
    }

    /// Set the diff algorithm.
    ///
    /// # Arguments
    ///
    /// * `algorithm` - The algorithm to use (Myers or Patience)
    #[must_use]
    pub fn with_algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set the default encoding for new files.
    ///
    /// # Arguments
    ///
    /// * `encoding` - The default encoding
    #[must_use]
    pub fn with_default_encoding(mut self, encoding: Encoding) -> Self {
        self.default_encoding = encoding;
        self
    }

    /// Set maximum file size for diffing.
    ///
    /// Files larger than this are treated as binary.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum size in bytes
    #[must_use]
    pub fn with_max_file_size(mut self, size: u64) -> Self {
        self.max_file_size = size;
        self
    }

    /// Set whether to skip binary files.
    ///
    /// # Arguments
    ///
    /// * `skip` - Whether to skip binary files
    #[must_use]
    pub fn with_skip_binary(mut self, skip: bool) -> Self {
        self.skip_binary = skip;
        self
    }

    /// Set whether to record empty files.
    ///
    /// # Arguments
    ///
    /// * `record` - Whether to record empty files
    #[must_use]
    pub fn record_empty_files(mut self, record: bool) -> Self {
        self.record_empty_files = record;
        self
    }

    /// Set the number of context lines.
    ///
    /// # Arguments
    ///
    /// * `lines` - Number of context lines for diff display
    #[must_use]
    pub fn context_lines(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Set the target stack.
    ///
    /// If not set, uses the current stack.
    ///
    /// # Arguments
    ///
    /// * `stack` - Stack name
    #[must_use]
    pub fn stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Set the change message.
    ///
    /// This is a convenience for setting the message without building a full header.
    ///
    /// # Arguments
    ///
    /// * `message` - The change message
    #[must_use]
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Set whether to apply the change after recording.
    ///
    /// Default is true.
    ///
    /// # Arguments
    ///
    /// * `apply` - Whether to apply after recording
    #[must_use]
    pub fn apply_after_record(mut self, apply: bool) -> Self {
        self.apply_after_record = apply;
        self
    }

    /// Set whether to save the change to the store.
    ///
    /// Default is true.
    ///
    /// # Arguments
    ///
    /// * `save` - Whether to save to store
    #[must_use]
    pub fn save_to_store(mut self, save: bool) -> Self {
        self.save_to_store = save;
        self
    }

    /// Set opaque metadata bytes for `HashedChange.metadata`.
    ///
    /// These bytes are included in the change's cryptographic hash,
    /// making them tamper-evident. Used by `atomic-agent` to embed
    /// the `SessionEnvelope` (turn number, session ID, timing, files).
    ///
    /// # Arguments
    ///
    /// * `bytes` - Raw bytes to store in `HashedChange.metadata`
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::record::RecordOptions;
    ///
    /// let options = RecordOptions::new()
    ///     .metadata_bytes(vec![0x41, 0x54, 0x53, 0x45]); // "ATSE" magic
    /// ```
    #[must_use]
    pub fn metadata_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.metadata_bytes = bytes;
        self
    }

    /// Get the metadata bytes.
    #[must_use]
    pub fn get_metadata_bytes(&self) -> &[u8] {
        &self.metadata_bytes
    }

    /// Set AI provenance information for this change.
    ///
    /// Use this when recording changes that were assisted by AI tools.
    /// The provenance information will be stored in the change and included
    /// in its cryptographic hash.
    ///
    /// # Arguments
    ///
    /// * `provenance` - Vector of provenance entries (one per AI interaction)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::record::RecordOptions;
    /// use atomic_core::change::{Provenance, AIVendor, AITool, SuggestionType};
    ///
    /// let provenance = Provenance::builder()
    ///     .vendor(AIVendor::Anthropic)
    ///     .model("claude-sonnet-4-20250514")
    ///     .tool(AITool::Editor("zed".to_string()))
    ///     .suggestion_type(SuggestionType::Collaborative)
    ///     .build();
    ///
    /// let options = RecordOptions::new()
    ///     .provenance(vec![provenance]);
    /// ```
    #[must_use]
    pub fn provenance(mut self, provenance: Vec<Provenance>) -> Self {
        self.provenance = provenance;
        self
    }

    /// Add a single provenance entry.
    ///
    /// This is a convenience method for adding one AI interaction's metadata.
    ///
    /// # Arguments
    ///
    /// * `entry` - The provenance entry to add
    #[must_use]
    pub fn add_provenance(mut self, entry: Provenance) -> Self {
        self.provenance.push(entry);
        self
    }

    // Getters

    /// Get the paths to record.
    #[must_use]
    pub fn get_paths(&self) -> &[String] {
        &self.paths
    }

    /// Get whether to record all changes.
    #[must_use]
    pub fn all(&self) -> bool {
        self.all
    }

    /// Get the diff algorithm.
    #[must_use]
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Get the default encoding.
    #[must_use]
    pub fn default_encoding(&self) -> Encoding {
        self.default_encoding
    }

    /// Get the maximum file size.
    #[must_use]
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size
    }

    /// Get whether to skip binary files.
    #[must_use]
    pub fn skip_binary(&self) -> bool {
        self.skip_binary
    }

    /// Get whether to record empty files.
    #[must_use]
    pub fn get_record_empty_files(&self) -> bool {
        self.record_empty_files
    }

    /// Get the number of context lines.
    #[must_use]
    pub fn get_context_lines(&self) -> usize {
        self.context_lines
    }

    /// Get the target stack.
    #[must_use]
    pub fn get_stack(&self) -> Option<&str> {
        self.stack.as_deref()
    }

    /// Get the change message.
    #[must_use]
    pub fn get_message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Get whether to apply after recording.
    #[must_use]
    pub fn get_apply_after_record(&self) -> bool {
        self.apply_after_record
    }

    /// Get whether to save to store.
    #[must_use]
    pub fn get_save_to_store(&self) -> bool {
        self.save_to_store
    }

    /// Get the AI provenance information.
    #[must_use]
    pub fn get_provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Check if this change has AI provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        !self.provenance.is_empty()
    }

    /// Check if a path should be included.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to check
    ///
    /// # Returns
    ///
    /// True if the path should be recorded.
    #[must_use]
    pub fn should_include(&self, path: &str) -> bool {
        if self.all || self.paths.is_empty() {
            return true;
        }
        self.paths
            .iter()
            .any(|p| path == p || path.starts_with(&format!("{}/", p)))
    }

    /// Convert to core recording options.
    #[must_use]
    pub fn to_core_options(&self) -> CoreRecordingOptions {
        CoreRecordingOptions::new()
            .algorithm(self.algorithm)
            .default_encoding(self.default_encoding)
            .max_file_size(self.max_file_size as usize)
            .skip_binary(self.skip_binary)
            .record_empty_files(self.record_empty_files)
            .context_lines(self.context_lines)
    }

    /// Convert to assembly options.
    #[must_use]
    pub fn to_assembly_options(&self) -> AssemblyOptions {
        AssemblyOptions::new()
            .include_empty_files(self.record_empty_files)
            .globalize_options(
                GlobalizeOptions::new()
                    .with_include_empty_files(self.record_empty_files)
                    .with_default_encoding(self.default_encoding),
            )
            .provenance(self.provenance.clone())
            .metadata_bytes(self.metadata_bytes.clone())
    }
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            all: false,
            algorithm: Algorithm::Myers,
            default_encoding: Encoding::Utf8,
            metadata_bytes: Vec::new(),
            max_file_size: Self::DEFAULT_MAX_FILE_SIZE,
            skip_binary: false,
            record_empty_files: false,
            context_lines: Self::DEFAULT_CONTEXT_LINES,
            stack: None,
            message: None,
            apply_after_record: true,
            save_to_store: true,
            provenance: Vec::new(),
        }
    }
}

// STATISTICS

/// Statistics about the recording process.
///
/// Includes both traditional graph_op-based statistics and CRDT token-level
/// statistics for fine-grained change tracking.
#[derive(Debug, Clone, Default)]
pub struct RecordStats {
    /// Number of files processed.
    pub files_processed: usize,

    /// Number of files recorded.
    pub files_recorded: usize,

    /// Number of directories recorded.
    pub directories_recorded: usize,

    /// Number of files skipped.
    pub files_skipped: usize,

    /// Number of hunks created.
    pub hunks_created: usize,

    /// Number of Insertion atoms created (content added to graph).
    pub vertices_added: usize,

    /// Number of EdgeUpdate atoms created (graph structure modified).
    pub edges_modified: usize,

    /// Total content bytes.
    pub content_bytes: u64,

    /// Number of dependencies.
    pub dependency_count: usize,

    /// Number of errors.
    pub errors: usize,

    // CRDT Token-Level Statistics (for fine-grained diff tracking)
    /// Number of lines added (CRDT BranchOp::Insert).
    pub lines_added: usize,

    /// Number of lines deleted (CRDT BranchOp::Delete).
    pub lines_deleted: usize,

    /// Number of lines modified (delete + insert at same position).
    pub lines_modified: usize,

    /// Number of tokens added (CRDT LeafOp::Insert).
    pub tokens_added: usize,

    /// Number of tokens deleted (CRDT LeafOp::Delete).
    pub tokens_deleted: usize,

    /// Number of tokens replaced (CRDT LeafOp::Replace - preserves ID for blame).
    pub tokens_replaced: usize,
}

impl RecordStats {
    /// Create new empty statistics.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any files or directories were recorded.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        self.files_recorded > 0 || self.directories_recorded > 0 || self.hunks_created > 0
    }

    /// Get total atoms (vertices + edge modifications).
    #[must_use]
    pub fn total_atoms(&self) -> usize {
        self.vertices_added + self.edges_modified
    }

    /// Check if any errors occurred.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    /// Get total line changes (added + deleted + modified).
    #[must_use]
    pub fn total_line_changes(&self) -> usize {
        self.lines_added + self.lines_deleted + self.lines_modified
    }

    /// Get total token operations (added + deleted + replaced).
    #[must_use]
    pub fn total_token_ops(&self) -> usize {
        self.tokens_added + self.tokens_deleted + self.tokens_replaced
    }

    /// Check if CRDT statistics are available.
    #[must_use]
    pub fn has_crdt_stats(&self) -> bool {
        self.lines_added > 0
            || self.lines_deleted > 0
            || self.lines_modified > 0
            || self.tokens_added > 0
            || self.tokens_deleted > 0
            || self.tokens_replaced > 0
    }
}

impl fmt::Display for RecordStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Basic stats
        write!(
            f,
            "{} file(s), {} graph_op(s), +{} vertices, ~{} edges, {} bytes",
            self.files_recorded,
            self.hunks_created,
            self.vertices_added,
            self.edges_modified,
            self.content_bytes
        )?;

        // CRDT token-level stats if available
        if self.has_crdt_stats() {
            write!(
                f,
                " | +{} -{} ~{} lines, +{} -{} ~{} tokens",
                self.lines_added,
                self.lines_deleted,
                self.lines_modified,
                self.tokens_added,
                self.tokens_deleted,
                self.tokens_replaced
            )?;
        }

        Ok(())
    }
}

// RESULT

/// Result of recording changes.
#[derive(Debug)]
pub struct RecordOutcome {
    /// The recorded change.
    change: Change,

    /// Hash of the change.
    hash: Hash,

    /// Recording statistics.
    stats: RecordStats,

    /// Whether the change was saved to the store.
    saved: bool,

    /// Whether the change was applied.
    applied: bool,

    /// New Merkle state after application (if applied).
    new_state: Option<Merkle>,

    /// Files that were recorded.
    recorded_files: Vec<String>,

    /// Files that were deleted.
    deleted_files: Vec<String>,

    /// Files that were skipped.
    skipped_files: Vec<String>,

    /// Non-fatal errors that occurred.
    errors: Vec<(String, String)>,

    /// The original serialized V3 bytes from the first serialize() call.
    ///
    /// Stored so that `save_change` can write the exact bytes to disk
    /// instead of re-serializing (which may produce a different hash due
    /// to different hash table ordering or chunk boundaries).
    ///
    /// This ensures the hash on disk matches the hash registered in the
    /// pristine graph, preventing "change not found" errors on push.
    v3_bytes: Option<Vec<u8>>,
}

impl RecordOutcome {
    /// Create a new record outcome.
    pub fn new(change: Change, hash: Hash, stats: RecordStats) -> Self {
        Self {
            change,
            hash,
            stats,
            saved: false,
            applied: false,
            new_state: None,
            recorded_files: Vec::new(),
            deleted_files: Vec::new(),
            skipped_files: Vec::new(),
            errors: Vec::new(),
            v3_bytes: None,
        }
    }

    /// Get the recorded change.
    #[must_use]
    pub fn change(&self) -> &Change {
        &self.change
    }

    /// Get a mutable reference to the recorded change.
    ///
    /// Used by `atomic-agent` to attach unhashed data (transcript, reasoning)
    /// to the change after recording.
    #[must_use]
    pub fn change_mut(&mut self) -> &mut Change {
        &mut self.change
    }

    /// Take ownership of the change.
    #[must_use]
    pub fn into_change(self) -> Change {
        self.change
    }

    /// Get the change hash.
    #[must_use]
    pub fn hash(&self) -> &Hash {
        &self.hash
    }

    /// Get the recording statistics.
    #[must_use]
    pub fn stats(&self) -> &RecordStats {
        &self.stats
    }

    /// Check if the change was saved.
    #[must_use]
    pub fn was_saved(&self) -> bool {
        self.saved
    }

    /// Check if the change was applied.
    #[must_use]
    pub fn was_applied(&self) -> bool {
        self.applied
    }

    /// Get the new state (if applied).
    #[must_use]
    pub fn new_state(&self) -> Option<Merkle> {
        self.new_state
    }

    /// Get the recorded files.
    #[must_use]
    pub fn recorded_files(&self) -> &[String] {
        &self.recorded_files
    }

    /// Get the deleted files.
    #[must_use]
    pub fn deleted_files(&self) -> &[String] {
        &self.deleted_files
    }

    /// Get the skipped files.
    #[must_use]
    pub fn skipped_files(&self) -> &[String] {
        &self.skipped_files
    }

    /// Get any errors that occurred.
    #[must_use]
    pub fn errors(&self) -> &[(String, String)] {
        &self.errors
    }

    /// Check if there were errors.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Mark as saved.
    pub fn set_saved(&mut self, saved: bool) {
        self.saved = saved;
    }

    /// Mark as applied with new state.
    pub fn set_applied(&mut self, state: Merkle) {
        self.applied = true;
        self.new_state = Some(state);
    }

    /// Add a recorded file path.
    pub fn add_recorded_file(&mut self, path: String) {
        self.recorded_files.push(path);
    }

    /// Add a deleted file path.
    pub fn add_deleted_file(&mut self, path: String) {
        self.deleted_files.push(path);
    }

    /// Add a skipped file path.
    pub fn add_skipped_file(&mut self, path: String) {
        self.skipped_files.push(path);
    }

    /// Add an error.
    /// Store the original V3 serialized bytes for hash-stable saving.
    ///
    /// Called by `Repository::record()` after the first `serialize()` call.
    /// These bytes are later written directly to disk by `save_change_bytes()`
    /// to avoid the hash mismatch caused by re-serialization.
    pub fn set_v3_bytes(&mut self, bytes: Vec<u8>) {
        self.v3_bytes = Some(bytes);
    }

    /// Get the original V3 bytes, if stored.
    pub fn v3_bytes(&self) -> Option<&[u8]> {
        self.v3_bytes.as_deref()
    }

    pub fn add_error(&mut self, path: String, error: String) {
        self.errors.push((path, error));
    }
}

impl fmt::Display for RecordOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Recorded change {} ({})",
            self.hash.to_base32(),
            self.stats
        )?;

        if self.saved {
            write!(f, " [saved]")?;
        }
        if self.applied {
            write!(f, " [applied]")?;
        }

        Ok(())
    }
}

// HELPER FUNCTIONS

/// Build a ChangeHeader from options and explicit header.
///
/// The explicit header takes precedence, but the options message can
/// override an empty header message.
pub fn build_header(header: ChangeHeader, options: &RecordOptions) -> ChangeHeader {
    let mut result = header;

    // Use message from options if header message is empty
    if result.message.is_empty() {
        if let Some(msg) = options.get_message() {
            result.message = msg.to_string();
        }
    }

    result
}

/// Filter files based on record options.
pub fn filter_files<'a>(
    files: &'a [FileStatusEntry],
    options: &RecordOptions,
) -> Vec<&'a FileStatusEntry> {
    files
        .iter()
        .filter(|f| {
            // Must be a recordable change (modified, added, deleted)
            matches!(
                f.status(),
                FileStatus::Modified | FileStatus::Added | FileStatus::Deleted
            )
        })
        .filter(|f| options.should_include(&f.path().to_string_lossy()))
        .collect()
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    // RecordOptions Tests

    #[test]
    fn test_options_new_returns_defaults() {
        let opts = RecordOptions::new();
        assert!(opts.get_paths().is_empty());
        assert!(!opts.all());
        assert_eq!(opts.algorithm(), Algorithm::Myers);
        assert_eq!(opts.default_encoding(), Encoding::Utf8);
        assert_eq!(opts.max_file_size(), RecordOptions::DEFAULT_MAX_FILE_SIZE);
        assert!(!opts.skip_binary());
        assert!(!opts.get_record_empty_files());
        assert_eq!(
            opts.get_context_lines(),
            RecordOptions::DEFAULT_CONTEXT_LINES
        );
        assert!(opts.get_stack().is_none());
        assert!(opts.get_message().is_none());
        assert!(opts.get_apply_after_record());
        assert!(opts.get_save_to_store());
    }

    #[test]
    fn test_options_default() {
        let opts = RecordOptions::default();
        assert!(opts.get_paths().is_empty());
        assert!(!opts.all());
    }

    #[test]
    fn test_options_paths() {
        let opts = RecordOptions::new().paths(vec!["src/main.rs", "src/lib.rs"]);
        assert_eq!(opts.get_paths().len(), 2);
        assert_eq!(opts.get_paths()[0], "src/main.rs");
    }

    #[test]
    fn test_options_add_path() {
        let opts = RecordOptions::new()
            .add_path("src/main.rs")
            .add_path("src/lib.rs");
        assert_eq!(opts.get_paths().len(), 2);
    }

    #[test]
    fn test_options_all() {
        let opts = RecordOptions::new().with_all(true);
        assert!(opts.all());
    }

    #[test]
    fn test_options_algorithm() {
        let opts = RecordOptions::new().with_algorithm(Algorithm::Patience);
        assert_eq!(opts.algorithm(), Algorithm::Patience);
    }

    #[test]
    fn test_options_default_encoding() {
        let opts = RecordOptions::new().with_default_encoding(Encoding::Binary);
        assert_eq!(opts.default_encoding(), Encoding::Binary);
    }

    #[test]
    fn test_options_max_file_size() {
        let opts = RecordOptions::new().with_max_file_size(1024);
        assert_eq!(opts.max_file_size(), 1024);
    }

    #[test]
    fn test_options_skip_binary() {
        let opts = RecordOptions::new().with_skip_binary(true);
        assert!(opts.skip_binary());
    }

    #[test]
    fn test_options_record_empty_files() {
        let opts = RecordOptions::new().record_empty_files(true);
        assert!(opts.get_record_empty_files());
    }

    #[test]
    fn test_options_context_lines() {
        let opts = RecordOptions::new().context_lines(5);
        assert_eq!(opts.get_context_lines(), 5);
    }

    #[test]
    fn test_options_stack() {
        let opts = RecordOptions::new().stack("feature");
        assert_eq!(opts.get_stack(), Some("feature"));
    }

    #[test]
    fn test_options_message() {
        let opts = RecordOptions::new().message("Test message");
        assert_eq!(opts.get_message(), Some("Test message"));
    }

    #[test]
    fn test_options_apply_after_record() {
        let opts = RecordOptions::new().apply_after_record(false);
        assert!(!opts.get_apply_after_record());
    }

    #[test]
    fn test_options_save_to_store() {
        let opts = RecordOptions::new().save_to_store(false);
        assert!(!opts.get_save_to_store());
    }

    #[test]
    fn test_options_builder_chain() {
        let opts = RecordOptions::new()
            .paths(vec!["src/"])
            .with_all(false)
            .with_algorithm(Algorithm::Patience)
            .with_max_file_size(1024 * 1024)
            .with_skip_binary(true)
            .message("Test change")
            .stack("feature");

        assert_eq!(opts.get_paths().len(), 1);
        assert!(!opts.all());
        assert_eq!(opts.algorithm(), Algorithm::Patience);
        assert!(opts.skip_binary());
        assert_eq!(opts.get_message(), Some("Test change"));
        assert_eq!(opts.get_stack(), Some("feature"));
    }

    #[test]
    fn test_options_should_include_all() {
        let opts = RecordOptions::new().with_all(true);
        assert!(opts.should_include("any/path/file.rs"));
    }

    #[test]
    fn test_options_should_include_empty_paths() {
        let opts = RecordOptions::new();
        assert!(opts.should_include("any/path/file.rs"));
    }

    #[test]
    fn test_options_should_include_specific_path() {
        let opts = RecordOptions::new().paths(vec!["src/main.rs"]);
        assert!(opts.should_include("src/main.rs"));
        assert!(!opts.should_include("src/lib.rs"));
    }

    #[test]
    fn test_options_should_include_directory() {
        let opts = RecordOptions::new().paths(vec!["src"]);
        assert!(opts.should_include("src/main.rs"));
        assert!(opts.should_include("src/lib.rs"));
        assert!(!opts.should_include("tests/test.rs"));
    }

    #[test]
    fn test_options_clone() {
        let opts1 = RecordOptions::new().message("test");
        let opts2 = opts1.clone();
        assert_eq!(opts2.get_message(), Some("test"));
    }

    #[test]
    fn test_options_debug() {
        let opts = RecordOptions::new();
        let debug = format!("{:?}", opts);
        assert!(debug.contains("RecordOptions"));
    }

    // RecordError Tests

    #[test]
    fn test_error_nothing_to_record() {
        let err = RecordError::NothingToRecord;
        let msg = format!("{}", err);
        assert!(msg.contains("Nothing to record"));
    }

    #[test]
    fn test_error_file_not_found() {
        let err = RecordError::FileNotFound {
            path: "test.rs".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
    }

    #[test]
    fn test_error_file_not_tracked() {
        let err = RecordError::FileNotTracked {
            path: "untracked.rs".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("untracked.rs"));
    }

    #[test]
    fn test_error_file_too_large() {
        let err = RecordError::FileTooLarge {
            path: "big.bin".to_string(),
            size: 100_000_000,
            limit: 10_000_000,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("big.bin"));
        assert!(msg.contains("100000000"));
    }

    // RecordStats Tests

    #[test]
    fn test_stats_new() {
        let stats = RecordStats::new();
        assert_eq!(stats.files_processed, 0);
        assert_eq!(stats.files_recorded, 0);
        assert_eq!(stats.hunks_created, 0);
    }

    #[test]
    fn test_stats_has_changes() {
        let mut stats = RecordStats::new();
        assert!(!stats.has_changes());

        stats.files_recorded = 1;
        assert!(stats.has_changes());
    }

    #[test]
    fn test_stats_total_atoms() {
        let mut stats = RecordStats::new();
        stats.vertices_added = 10;
        stats.edges_modified = 5;
        assert_eq!(stats.total_atoms(), 15);
    }

    #[test]
    fn test_stats_has_errors() {
        let mut stats = RecordStats::new();
        assert!(!stats.has_errors());

        stats.errors = 1;
        assert!(stats.has_errors());
    }

    #[test]
    fn test_stats_display() {
        let mut stats = RecordStats::new();
        stats.files_processed = 10;
        stats.files_recorded = 5;
        stats.files_skipped = 5;
        stats.hunks_created = 15;
        stats.vertices_added = 100;
        stats.edges_modified = 50;
        stats.content_bytes = 2048;
        stats.dependency_count = 2;
        stats.errors = 0;

        let display = format!("{}", stats);
        assert!(display.contains("5 file(s)"));
        assert!(display.contains("15 graph_op(s)"));
        assert!(display.contains("+100 vertices"));
        assert!(display.contains("~50 edges"));
        assert!(display.contains("2048 bytes"));
    }

    #[test]
    fn test_stats_crdt_display() {
        let mut stats = RecordStats::new();
        stats.files_recorded = 2;
        stats.hunks_created = 3;
        stats.vertices_added = 10;
        stats.edges_modified = 5;
        stats.content_bytes = 512;
        stats.lines_added = 15;
        stats.lines_deleted = 3;
        stats.lines_modified = 2;
        stats.tokens_added = 45;
        stats.tokens_deleted = 8;
        stats.tokens_replaced = 4;

        let display = format!("{}", stats);
        // Basic stats
        assert!(display.contains("2 file(s)"));
        assert!(display.contains("3 graph_op(s)"));
        // CRDT stats
        assert!(display.contains("+15 -3 ~2 lines"));
        assert!(display.contains("+45 -8 ~4 tokens"));
    }

    #[test]
    fn test_stats_total_line_changes() {
        let mut stats = RecordStats::new();
        stats.lines_added = 10;
        stats.lines_deleted = 5;
        stats.lines_modified = 3;
        assert_eq!(stats.total_line_changes(), 18);
    }

    #[test]
    fn test_stats_total_token_ops() {
        let mut stats = RecordStats::new();
        stats.tokens_added = 20;
        stats.tokens_deleted = 8;
        stats.tokens_replaced = 2;
        assert_eq!(stats.total_token_ops(), 30);
    }

    #[test]
    fn test_stats_has_crdt_stats() {
        let mut stats = RecordStats::new();
        assert!(!stats.has_crdt_stats());

        stats.lines_added = 1;
        assert!(stats.has_crdt_stats());

        let mut stats2 = RecordStats::new();
        stats2.tokens_added = 5;
        assert!(stats2.has_crdt_stats());
    }

    // RecordOutcome Tests

    #[test]
    fn test_outcome_new() {
        let header = ChangeHeader::builder().message("Test").build();
        let change = Change::empty(header);
        let hash = Hash::of(b"test");
        let stats = RecordStats::new();

        let outcome = RecordOutcome::new(change, hash, stats);
        assert!(!outcome.was_saved());
        assert!(!outcome.was_applied());
        assert!(outcome.new_state().is_none());
    }

    #[test]
    fn test_outcome_set_saved() {
        let header = ChangeHeader::builder().message("Test").build();
        let change = Change::empty(header);
        let hash = Hash::of(b"test");
        let stats = RecordStats::new();

        let mut outcome = RecordOutcome::new(change, hash, stats);
        outcome.set_saved(true);
        assert!(outcome.was_saved());
    }

    #[test]
    fn test_outcome_set_applied() {
        let header = ChangeHeader::builder().message("Test").build();
        let change = Change::empty(header);
        let hash = Hash::of(b"test");
        let stats = RecordStats::new();

        let mut outcome = RecordOutcome::new(change, hash, stats);
        let state = Merkle::of(b"state");
        outcome.set_applied(state);
        assert!(outcome.was_applied());
        assert_eq!(outcome.new_state(), Some(state));
    }

    #[test]
    fn test_outcome_add_files() {
        let header = ChangeHeader::builder().message("Test").build();
        let change = Change::empty(header);
        let hash = Hash::of(b"test");
        let stats = RecordStats::new();

        let mut outcome = RecordOutcome::new(change, hash, stats);
        outcome.add_recorded_file("src/main.rs".to_string());
        outcome.add_skipped_file("src/test.rs".to_string());

        assert_eq!(outcome.recorded_files().len(), 1);
        assert_eq!(outcome.skipped_files().len(), 1);
    }

    #[test]
    fn test_outcome_add_error() {
        let header = ChangeHeader::builder().message("Test").build();
        let change = Change::empty(header);
        let hash = Hash::of(b"test");
        let stats = RecordStats::new();

        let mut outcome = RecordOutcome::new(change, hash, stats);
        outcome.add_error("file.rs".to_string(), "read error".to_string());

        assert!(outcome.has_errors());
        assert_eq!(outcome.errors().len(), 1);
    }

    #[test]
    fn test_outcome_display() {
        let header = ChangeHeader::builder().message("Test").build();
        let change = Change::empty(header);
        let hash = Hash::of(b"test");
        let stats = RecordStats::new();

        let mut outcome = RecordOutcome::new(change, hash, stats);
        outcome.set_saved(true);
        outcome.set_applied(Merkle::of(b"state"));

        let display = format!("{}", outcome);
        assert!(display.contains("Recorded change"));
        assert!(display.contains("[saved]"));
        assert!(display.contains("[applied]"));
    }

    #[test]
    fn test_outcome_into_change() {
        let header = ChangeHeader::builder().message("Take me").build();
        let change = Change::empty(header);
        let hash = Hash::of(b"test");
        let stats = RecordStats::new();

        let outcome = RecordOutcome::new(change, hash, stats);
        let taken = outcome.into_change();
        assert_eq!(taken.message(), "Take me");
    }

    // Helper Function Tests

    #[test]
    fn test_build_header_with_options_message() {
        let header = ChangeHeader::builder().build(); // Empty message
        let options = RecordOptions::new().message("From options");
        let result = build_header(header, &options);
        assert_eq!(result.message, "From options");
    }

    #[test]
    fn test_build_header_preserves_header_message() {
        let header = ChangeHeader::builder().message("From header").build();
        let options = RecordOptions::new().message("From options");
        let result = build_header(header, &options);
        assert_eq!(result.message, "From header");
    }

    #[test]
    fn test_filter_files_empty() {
        let files: Vec<FileStatusEntry> = vec![];
        let options = RecordOptions::new();
        let filtered = filter_files(&files, &options);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_files_with_modified() {
        use std::path::PathBuf;

        let files = vec![
            FileStatusEntry::new(PathBuf::from("src/main.rs"), FileStatus::Modified),
            FileStatusEntry::new(PathBuf::from("README.md"), FileStatus::Clean),
            FileStatusEntry::new(PathBuf::from("src/lib.rs"), FileStatus::Added),
        ];
        let options = RecordOptions::new();
        let filtered = filter_files(&files, &options);
        assert_eq!(filtered.len(), 2);
    }

    // Provenance Tests

    #[test]
    fn test_options_provenance_default() {
        let opts = RecordOptions::new();
        assert!(!opts.has_provenance());
        assert!(opts.get_provenance().is_empty());
    }

    #[test]
    fn test_options_provenance_add_single() {
        use atomic_core::change::{AITool, AIVendor, SuggestionType};

        let prov = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .model("claude-sonnet-4-20250514")
            .tool(AITool::Editor("zed".to_string()))
            .suggestion_type(SuggestionType::Collaborative)
            .build();

        let opts = RecordOptions::new().add_provenance(prov);
        assert!(opts.has_provenance());
        assert_eq!(opts.get_provenance().len(), 1);
        assert_eq!(opts.get_provenance()[0].vendor, AIVendor::Anthropic);
    }

    #[test]
    fn test_options_provenance_set_vec() {
        use atomic_core::change::{AITool, AIVendor, SuggestionType};

        let prov1 = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .model("claude-sonnet-4-20250514")
            .build();

        let prov2 = Provenance::builder()
            .vendor(AIVendor::OpenAI)
            .model("gpt-4")
            .build();

        let opts = RecordOptions::new().provenance(vec![prov1, prov2]);
        assert!(opts.has_provenance());
        assert_eq!(opts.get_provenance().len(), 2);
    }

    #[test]
    fn test_options_provenance_builder_chain() {
        use atomic_core::change::{AITool, AIVendor, SuggestionType};

        let prov = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .model("claude-sonnet-4-20250514")
            .tool(AITool::Cli("atomic".to_string()))
            .suggestion_type(SuggestionType::Complete)
            .input_tokens(1000)
            .output_tokens(500)
            .cost_usd(0.015)
            .build();

        let opts = RecordOptions::new()
            .message("AI-assisted change")
            .with_all(true)
            .add_provenance(prov);

        assert!(opts.has_provenance());
        assert_eq!(opts.get_message(), Some("AI-assisted change"));
        assert!(opts.all());
    }

    #[test]
    fn test_options_to_assembly_options_includes_provenance() {
        use atomic_core::change::{AITool, AIVendor, SuggestionType};

        let prov = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .model("claude-sonnet-4-20250514")
            .build();

        let record_opts = RecordOptions::new().add_provenance(prov);
        let assembly_opts = record_opts.to_assembly_options();

        assert!(assembly_opts.has_provenance());
        assert_eq!(assembly_opts.get_provenance().len(), 1);
        assert_eq!(
            assembly_opts.get_provenance()[0].vendor,
            AIVendor::Anthropic
        );
    }

    #[test]
    fn test_options_clone_preserves_provenance() {
        use atomic_core::change::{AITool, AIVendor};

        let prov = Provenance::builder()
            .vendor(AIVendor::OpenAI)
            .model("gpt-4")
            .build();

        let opts1 = RecordOptions::new().add_provenance(prov);
        let opts2 = opts1.clone();

        assert!(opts2.has_provenance());
        assert_eq!(opts2.get_provenance().len(), 1);
        assert_eq!(opts2.get_provenance()[0].vendor, AIVendor::OpenAI);
    }
}
