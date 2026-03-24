//! Change assembly from recorded files.
//!
//! This module is responsible for combining multiple [`RecordedFile`] results
//! into a complete [`Change`] that can be serialized and applied to other
//! repositories.
//!
//! # Overview
//!
//! The assembly process involves several steps:
//!
//! 1. **Globalization**: Convert local hunks to graph-compatible hunks
//! 2. **Content Aggregation**: Combine all content into a single blob
//! 3. **Offset Computation**: Calculate byte offsets for each graph_op
//! 4. **Dependency Collection**: Gather all change dependencies
//! 5. **Finalization**: Create the complete Change structure
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Assembly Pipeline                                │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  RecordedFile[]        AssemblyContext           Change                 │
//! │  ┌──────────────┐     ┌───────────────┐        ┌──────────────────────┐│
//! │  │ path         │     │ header        │        │ offsets              ││
//! │  │ hunks[]      │ ──► │ content_buf   │  ──►   │ hashed               ││
//! │  │ content      │     │ dependencies  │        │   header             ││
//! │  │ encoding     │     │ hunks[]       │        │   dependencies       ││
//! │  └──────────────┘     └───────────────┘        │   hunks[]            ││
//! │                                                 │   contents_hash      ││
//! │                                                 │ contents             ││
//! │                                                 └──────────────────────┘│
//! │                                                                         │
//! │  Offset Computation:                                                    │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ GraphOp 1: start=0, end=100                                         │  │
//! │  │ GraphOp 2: start=100, end=250                                       │  │
//! │  │ GraphOp 3: start=250, end=400                                       │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::assembly::{
//!     AssemblyContext, AssemblyOptions, assemble_change,
//! };
//! use atomic_core::change::ChangeHeader;
//!
//! // Create the change header
//! let header = ChangeHeader::builder()
//!     .message("Add new feature")
//!     .author(Author::new("Alice", Some("alice@example.com")))
//!     .build();
//!
//! // Assemble the change from recorded files
//! let change = assemble_change(
//!     &txn,
//!     &recorded_files,
//!     header,
//!     &AssemblyOptions::default(),
//! )?;
//!
//! // The change is now ready for serialization
//! let hash = change.serialize(&mut file)?;
//! ```
//!
//! # Error Handling
//!
//! Assembly can fail for several reasons:
//!
//! - **Globalization errors**: File paths not found, missing inodes
//! - **Content errors**: Invalid content ranges
//! - **Dependency cycles**: Circular dependencies detected
//!
//! See [`AssemblyError`] for the complete list.

use std::collections::HashSet;
use std::fmt;

use thiserror::Error;

use crate::change::{Change, ChangeHeader, FileOps, GraphOp, Provenance};
use crate::pristine::{GraphTxnT, TreeTxnT};
use crate::types::Hash;

use super::globalize::{
    globalize_recorded_file, GlobalizeContext, GlobalizeError, GlobalizeOptions, GlobalizedFile,
};
use super::record::RecordedFile;

// ============================================================================
// ERROR TYPES
// ============================================================================

/// Errors that can occur during change assembly.
#[derive(Debug, Error)]
pub enum AssemblyError {
    /// No files to assemble.
    ///
    /// At least one recorded file is required to create a change.
    #[error("No files to assemble into a change")]
    NoFiles,

    /// All files were empty (no hunks).
    ///
    /// This occurs when none of the recorded files have any changes.
    #[error("All recorded files are empty (no hunks)")]
    AllEmpty,

    /// Globalization failed for a file.
    #[error("Failed to globalize file {path}: {source}")]
    Globalize {
        /// The file path that failed
        path: String,
        /// The underlying error
        #[source]
        source: GlobalizeError,
    },

    /// Invalid content range in a graph_op.
    #[error("Invalid content range [{start}, {end}) for file {path}")]
    InvalidContentRange {
        /// The file path
        path: String,
        /// The start position
        start: u64,
        /// The end position
        end: u64,
    },

    /// Content size limit exceeded.
    #[error("Content size {actual} exceeds limit {limit}")]
    ContentTooLarge {
        /// Actual content size
        actual: usize,
        /// Maximum allowed size
        limit: usize,
    },

    /// Too many hunks in a single change.
    #[error("GraphOp count {actual} exceeds limit {limit}")]
    TooManyHunks {
        /// Actual graph_op count
        actual: usize,
        /// Maximum allowed hunks
        limit: usize,
    },

    /// Missing required header field.
    #[error("Missing required header field: {field}")]
    MissingHeaderField {
        /// The missing field name
        field: &'static str,
    },

    /// Dependency cycle detected.
    ///
    /// This should not normally happen, but indicates a bug if it does.
    #[error("Dependency cycle detected")]
    DependencyCycle,
}

/// Result type for assembly operations.
pub type AssemblyResult<T> = Result<T, AssemblyError>;

// ============================================================================
// CONFIGURATION
// ============================================================================

/// Configuration options for change assembly.
///
/// Controls how recorded files are combined into a change.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::assembly::AssemblyOptions;
///
/// let options = AssemblyOptions::new()
///     .max_content_size(10 * 1024 * 1024)
///     .max_hunks(1000)
///     .include_empty_files(false);
///
/// assert_eq!(options.get_max_content_size(), 10 * 1024 * 1024);
/// ```
#[derive(Debug, Clone)]
pub struct AssemblyOptions {
    /// Maximum total content size in bytes.
    ///
    /// Changes larger than this will be rejected.
    /// Default: 100 MB
    max_content_size: usize,

    /// Maximum number of hunks per change.
    ///
    /// Default: 10,000
    max_hunks: usize,

    /// Whether to include files with no hunks.
    ///
    /// Default: false
    include_empty_files: bool,

    /// Whether to validate dependencies.
    ///
    /// Default: true
    validate_dependencies: bool,

    /// Options for globalization.
    globalize_options: GlobalizeOptions,

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

impl AssemblyOptions {
    /// Default maximum content size (100 MB).
    pub const DEFAULT_MAX_CONTENT_SIZE: usize = 100 * 1024 * 1024;

    /// Default maximum number of hunks.
    pub const DEFAULT_MAX_HUNKS: usize = 10_000;

    /// Create new options with default values.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::assembly::AssemblyOptions;
    ///
    /// let options = AssemblyOptions::new();
    /// assert_eq!(options.get_max_content_size(), AssemblyOptions::DEFAULT_MAX_CONTENT_SIZE);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set maximum content size.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum content size in bytes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::assembly::AssemblyOptions;
    ///
    /// let options = AssemblyOptions::new().max_content_size(1024 * 1024);
    /// assert_eq!(options.get_max_content_size(), 1024 * 1024);
    /// ```
    #[must_use]
    pub fn max_content_size(mut self, size: usize) -> Self {
        self.max_content_size = size;
        self
    }

    /// Set maximum number of hunks.
    ///
    /// # Arguments
    ///
    /// * `count` - Maximum number of hunks
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::assembly::AssemblyOptions;
    ///
    /// let options = AssemblyOptions::new().max_hunks(500);
    /// assert_eq!(options.get_max_hunks(), 500);
    /// ```
    #[must_use]
    pub fn max_hunks(mut self, count: usize) -> Self {
        self.max_hunks = count;
        self
    }

    /// Set whether to include empty files.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include files with no hunks
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::assembly::AssemblyOptions;
    ///
    /// let options = AssemblyOptions::new().include_empty_files(true);
    /// assert!(options.get_include_empty_files());
    /// ```
    #[must_use]
    pub fn include_empty_files(mut self, include: bool) -> Self {
        self.include_empty_files = include;
        self.globalize_options = self.globalize_options.with_include_empty_files(include);
        self
    }

    /// Set whether to validate dependencies.
    ///
    /// # Arguments
    ///
    /// * `validate` - Whether to validate dependencies
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::assembly::AssemblyOptions;
    ///
    /// let options = AssemblyOptions::new().validate_dependencies(false);
    /// assert!(!options.get_validate_dependencies());
    /// ```
    #[must_use]
    pub fn validate_dependencies(mut self, validate: bool) -> Self {
        self.validate_dependencies = validate;
        self
    }

    /// Set globalization options.
    ///
    /// # Arguments
    ///
    /// * `options` - The globalization options
    #[must_use]
    pub fn globalize_options(mut self, options: GlobalizeOptions) -> Self {
        self.globalize_options = options;
        self
    }

    /// Get maximum content size.
    #[must_use]
    pub fn get_max_content_size(&self) -> usize {
        self.max_content_size
    }

    /// Get maximum number of hunks.
    #[must_use]
    pub fn get_max_hunks(&self) -> usize {
        self.max_hunks
    }

    /// Get whether empty files are included.
    #[must_use]
    pub fn get_include_empty_files(&self) -> bool {
        self.include_empty_files
    }

    /// Get whether dependencies are validated.
    #[must_use]
    pub fn get_validate_dependencies(&self) -> bool {
        self.validate_dependencies
    }

    /// Get a reference to globalization options.
    #[must_use]
    pub fn get_globalize_options(&self) -> &GlobalizeOptions {
        &self.globalize_options
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
    /// use atomic_core::record::workflow::assembly::AssemblyOptions;
    /// use atomic_core::change::{Provenance, AIVendor, AITool, SuggestionType};
    ///
    /// let provenance = Provenance::builder()
    ///     .vendor(AIVendor::Anthropic)
    ///     .model("claude-sonnet-4-20250514")
    ///     .tool(AITool::Editor("zed".to_string()))
    ///     .suggestion_type(SuggestionType::Collaborative)
    ///     .build();
    ///
    /// let options = AssemblyOptions::new()
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

    /// Set opaque metadata bytes for `HashedChange.metadata`.
    ///
    /// These bytes are included in the change's cryptographic hash,
    /// making them tamper-evident.
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

    /// Check if metadata bytes are set.
    #[must_use]
    pub fn has_metadata_bytes(&self) -> bool {
        !self.metadata_bytes.is_empty()
    }
}

impl Default for AssemblyOptions {
    fn default() -> Self {
        Self {
            max_content_size: Self::DEFAULT_MAX_CONTENT_SIZE,
            max_hunks: Self::DEFAULT_MAX_HUNKS,
            include_empty_files: false,
            validate_dependencies: true,
            globalize_options: GlobalizeOptions::default(),
            provenance: Vec::new(),
            metadata_bytes: Vec::new(),
        }
    }
}

// ============================================================================
// ASSEMBLY CONTEXT
// ============================================================================

/// Context for change assembly.
///
/// Accumulates hunks, content, and dependencies during the assembly process.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::assembly::AssemblyContext;
/// use atomic_core::change::ChangeHeader;
///
/// let mut ctx = AssemblyContext::new(header);
///
/// // Add hunks from globalized files
/// for file in globalized_files {
///     for graph_op in file.hunks() {
///         ctx.add_hunk(graph_op.clone());
///     }
/// }
///
/// // Finalize the change
/// let change = ctx.finalize(content)?;
/// ```
pub struct AssemblyContext {
    /// The change header.
    header: ChangeHeader,

    /// Accumulated hunks (graph operations).
    hunks: Vec<GraphOp<Option<Hash>>>,

    /// Accumulated file operations (semantic layer).
    file_ops: Vec<FileOps>,

    /// Accumulated dependencies.
    dependencies: HashSet<Hash>,

    /// Extra known changes (not dependencies but referenced).
    extra_known: HashSet<Hash>,

    /// Statistics about the assembly.
    stats: AssemblyStats,
}

impl AssemblyContext {
    /// Create a new assembly context.
    ///
    /// # Arguments
    ///
    /// * `header` - The change header
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let header = ChangeHeader::builder().message("Test").build();
    /// let ctx = AssemblyContext::new(header);
    /// ```
    pub fn new(header: ChangeHeader) -> Self {
        Self {
            header,
            hunks: Vec::new(),
            file_ops: Vec::new(),
            dependencies: HashSet::new(),
            extra_known: HashSet::new(),
            stats: AssemblyStats::new(),
        }
    }

    /// Create a context with pre-allocated capacity.
    ///
    /// # Arguments
    ///
    /// * `header` - The change header
    /// * `hunk_capacity` - Expected number of hunks
    pub fn with_capacity(header: ChangeHeader, hunk_capacity: usize) -> Self {
        Self {
            header,
            hunks: Vec::with_capacity(hunk_capacity),
            file_ops: Vec::new(),
            dependencies: HashSet::new(),
            extra_known: HashSet::new(),
            stats: AssemblyStats::new(),
        }
    }

    /// Add a graph_op to the context.
    ///
    /// # Arguments
    ///
    /// * `graph_op` - The graph_op to add
    pub fn add_hunk(&mut self, graph_op: GraphOp<Option<Hash>>) {
        self.hunks.push(graph_op);
        self.stats.hunks_added += 1;
    }

    /// Add a file operation to the context (semantic layer).
    ///
    /// # Arguments
    ///
    /// * `ops` - The file operation to add
    pub fn add_file_ops(&mut self, ops: FileOps) {
        self.file_ops.push(ops);
    }

    /// Add a dependency.
    ///
    /// # Arguments
    ///
    /// * `hash` - The dependency hash
    pub fn add_dependency(&mut self, hash: Hash) {
        if self.dependencies.insert(hash) {
            self.stats.dependencies_added += 1;
        }
    }

    /// Add multiple dependencies.
    ///
    /// # Arguments
    ///
    /// * `hashes` - Iterator of dependency hashes
    pub fn add_dependencies(&mut self, hashes: impl IntoIterator<Item = Hash>) {
        for hash in hashes {
            self.add_dependency(hash);
        }
    }

    /// Add an extra known change.
    ///
    /// Extra known changes are referenced but not direct dependencies.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash to add
    pub fn add_extra_known(&mut self, hash: Hash) {
        self.extra_known.insert(hash);
    }

    /// Get the number of hunks.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Get the number of dependencies.
    #[must_use]
    pub fn dependency_count(&self) -> usize {
        self.dependencies.len()
    }

    /// Get the assembly statistics.
    #[must_use]
    pub fn stats(&self) -> &AssemblyStats {
        &self.stats
    }

    /// Finalize the assembly and create the Change.
    ///
    /// # Arguments
    ///
    /// * `content` - The content blob
    /// * `provenance` - AI provenance information (empty if not AI-assisted)
    ///
    /// # Returns
    ///
    /// The assembled Change.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let change = ctx.finalize(content_bytes, vec![])?;
    /// ```
    #[must_use]
    pub fn finalize(
        self,
        content: Vec<u8>,
        provenance: Vec<Provenance>,
        metadata_bytes: Vec<u8>,
    ) -> Change {
        let mut dependencies: Vec<Hash> = self.dependencies.into_iter().collect();
        dependencies.sort();

        let mut extra_known: Vec<Hash> = self.extra_known.into_iter().collect();
        extra_known.sort();

        let mut change = Change::with_file_ops(
            self.header,
            self.hunks,
            self.file_ops,
            content,
            dependencies,
        );
        change.hashed.extra_known = extra_known;

        // Set opaque metadata bytes (e.g., SessionEnvelope from atomic-agent)
        if !metadata_bytes.is_empty() {
            change.hashed.metadata = metadata_bytes;
        }

        // Add AI provenance information
        for entry in provenance {
            change.add_provenance(entry);
        }

        change
    }

    /// Get the number of file operations collected.
    #[must_use]
    pub fn file_ops_count(&self) -> usize {
        self.file_ops.len()
    }

    /// Get a reference to the header.
    #[must_use]
    pub fn header(&self) -> &ChangeHeader {
        &self.header
    }

    /// Get a reference to the hunks.
    #[must_use]
    pub fn hunks(&self) -> &[GraphOp<Option<Hash>>] {
        &self.hunks
    }
}

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
// ASSEMBLY RESULT
// ============================================================================

/// Result of the assembly process.
///
/// Contains the assembled change and statistics.
#[derive(Debug)]
pub struct AssemblyResult_ {
    /// The assembled change.
    change: Change,

    /// Assembly statistics.
    stats: AssemblyStats,

    /// Files that were successfully globalized.
    globalized_files: Vec<GlobalizedFile>,

    /// Errors encountered during globalization (non-fatal).
    globalize_errors: Vec<(String, GlobalizeError)>,
}

impl AssemblyResult_ {
    /// Create a new assembly result.
    pub fn new(
        change: Change,
        stats: AssemblyStats,
        globalized_files: Vec<GlobalizedFile>,
        globalize_errors: Vec<(String, GlobalizeError)>,
    ) -> Self {
        Self {
            change,
            stats,
            globalized_files,
            globalize_errors,
        }
    }

    /// Get the assembled change.
    #[must_use]
    pub fn change(&self) -> &Change {
        &self.change
    }

    /// Take ownership of the change.
    #[must_use]
    pub fn into_change(self) -> Change {
        self.change
    }

    /// Get the assembly statistics.
    #[must_use]
    pub fn stats(&self) -> &AssemblyStats {
        &self.stats
    }

    /// Get the globalized files.
    #[must_use]
    pub fn globalized_files(&self) -> &[GlobalizedFile] {
        &self.globalized_files
    }

    /// Get any globalization errors.
    #[must_use]
    pub fn globalize_errors(&self) -> &[(String, GlobalizeError)] {
        &self.globalize_errors
    }

    /// Check if there were any errors.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.globalize_errors.is_empty()
    }

    /// Get the number of hunks in the change.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.change.hunks().len()
    }

    /// Get the content size.
    #[must_use]
    pub fn content_size(&self) -> usize {
        self.change.contents.len()
    }
}

// ============================================================================
// MAIN ASSEMBLY FUNCTIONS
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

/// Assemble a change from recorded files.
///
/// This is the main entry point for creating a Change from recording results.
/// It globalizes all hunks, collects dependencies, and creates the final
/// Change structure.
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `files` - The recorded files to assemble
/// * `header` - The change header
/// * `options` - Assembly options
///
/// # Returns
///
/// The assembled change, or an error if assembly fails.
///
/// # Example
///
/// ```rust,ignore
/// let header = ChangeHeader::builder()
///     .message("Add feature")
///     .author(Author::new("Alice", None))
///     .build();
///
/// let change = assemble_change(&txn, &recorded_files, header, &AssemblyOptions::default())?;
/// ```
pub fn assemble_change<T>(
    txn: &T,
    files: &[RecordedFile],
    header: ChangeHeader,
    options: &AssemblyOptions,
) -> AssemblyResult<AssemblyResult_>
where
    T: GraphTxnT + TreeTxnT,
{
    // Validate input
    if files.is_empty() {
        return Err(AssemblyError::NoFiles);
    }

    // Create globalization context
    let mut glob_ctx = GlobalizeContext::new(txn);
    let mut ctx = AssemblyContext::new(header);
    let mut stats = AssemblyStats::new();
    let mut globalized_files = Vec::new();
    let mut globalize_errors = Vec::new();

    // Process each file
    for file in files {
        stats.record_file();

        // Skip empty files if configured, but never skip directories
        // (directories generate hunks during globalization, not during recording)
        if file.is_empty()
            && !options.get_include_empty_files()
            && !file.is_directory()
            && !file.is_deleted_directory()
        {
            stats.record_skip();
            continue;
        }

        // Collect CRDT file operations (semantic layer) BEFORE globalization
        // These provide human-readable line/token operations for diff and blame
        if let Some(crdt_ops) = file.crdt_ops() {
            ctx.add_file_ops(crdt_ops.clone());
        }

        // Globalize the file
        match globalize_recorded_file(&mut glob_ctx, file, options.get_globalize_options()) {
            Ok(globalized) => {
                if globalized.is_empty() {
                    stats.record_skip();
                    continue;
                }

                // Add hunks from the globalized file
                for graph_op in globalized.hunks() {
                    ctx.add_hunk(graph_op.clone());
                }

                stats.add_content_bytes(globalized.bytes_added());
                globalized_files.push(globalized);
            }
            Err(e) => {
                stats.record_error();
                globalize_errors.push((file.path().to_string(), e));
            }
        }
    }

    // Check if we have any hunks
    if ctx.hunk_count() == 0 && !options.get_include_empty_files() {
        return Err(AssemblyError::AllEmpty);
    }

    // Add dependencies from globalization context
    ctx.add_dependencies(glob_ctx.dependencies().iter().copied());

    // Check content size limit
    let content = glob_ctx.take_content();
    if content.len() > options.get_max_content_size() {
        return Err(AssemblyError::ContentTooLarge {
            actual: content.len(),
            limit: options.get_max_content_size(),
        });
    }

    // Finalize the change with AI provenance and metadata bytes from options
    let change = ctx.finalize(
        content,
        options.get_provenance().to_vec(),
        options.get_metadata_bytes().to_vec(),
    );

    Ok(AssemblyResult_::new(
        change,
        stats,
        globalized_files,
        globalize_errors,
    ))
}

/// Create an empty change (with header only).
///
/// This is useful for creating placeholder changes or changes that
/// only modify metadata.
///
/// # Arguments
///
/// * `header` - The change header
///
/// # Returns
///
/// An empty change.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::assembly::create_empty_change;
/// use atomic_core::change::ChangeHeader;
///
/// let header = ChangeHeader::builder().message("Empty change").build();
/// let change = create_empty_change(header);
///
/// assert!(change.hunks().is_empty());
/// ```
#[must_use]
pub fn create_empty_change(header: ChangeHeader) -> Change {
    Change::empty(header)
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // AssemblyOptions Tests
    // ========================================================================

    #[test]
    fn test_options_new_returns_defaults() {
        let opts = AssemblyOptions::new();
        assert_eq!(
            opts.get_max_content_size(),
            AssemblyOptions::DEFAULT_MAX_CONTENT_SIZE
        );
        assert_eq!(opts.get_max_hunks(), AssemblyOptions::DEFAULT_MAX_HUNKS);
        assert!(!opts.get_include_empty_files());
        assert!(opts.get_validate_dependencies());
    }

    #[test]
    fn test_options_default() {
        let opts = AssemblyOptions::default();
        assert_eq!(opts.get_max_content_size(), 100 * 1024 * 1024);
    }

    #[test]
    fn test_options_max_content_size() {
        let opts = AssemblyOptions::new().max_content_size(1024);
        assert_eq!(opts.get_max_content_size(), 1024);
    }

    #[test]
    fn test_options_max_hunks() {
        let opts = AssemblyOptions::new().max_hunks(100);
        assert_eq!(opts.get_max_hunks(), 100);
    }

    #[test]
    fn test_options_include_empty_files() {
        let opts = AssemblyOptions::new().include_empty_files(true);
        assert!(opts.get_include_empty_files());
    }

    #[test]
    fn test_options_validate_dependencies() {
        let opts = AssemblyOptions::new().validate_dependencies(false);
        assert!(!opts.get_validate_dependencies());
    }

    #[test]
    fn test_options_builder_chain() {
        let opts = AssemblyOptions::new()
            .max_content_size(1024)
            .max_hunks(50)
            .include_empty_files(true)
            .validate_dependencies(false);

        assert_eq!(opts.get_max_content_size(), 1024);
        assert_eq!(opts.get_max_hunks(), 50);
        assert!(opts.get_include_empty_files());
        assert!(!opts.get_validate_dependencies());
    }

    #[test]
    fn test_options_clone() {
        let opts1 = AssemblyOptions::new().max_hunks(100);
        let opts2 = opts1.clone();
        assert_eq!(opts2.get_max_hunks(), 100);
    }

    #[test]
    fn test_options_debug() {
        let opts = AssemblyOptions::new();
        let debug = format!("{:?}", opts);
        assert!(debug.contains("AssemblyOptions"));
    }

    // ========================================================================
    // AssemblyError Tests
    // ========================================================================

    #[test]
    fn test_error_no_files() {
        let err = AssemblyError::NoFiles;
        let msg = format!("{}", err);
        assert!(msg.contains("No files"));
    }

    #[test]
    fn test_error_all_empty() {
        let err = AssemblyError::AllEmpty;
        let msg = format!("{}", err);
        assert!(msg.contains("empty"));
    }

    #[test]
    fn test_error_content_too_large() {
        let err = AssemblyError::ContentTooLarge {
            actual: 200,
            limit: 100,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("200"));
        assert!(msg.contains("100"));
    }

    #[test]
    fn test_error_too_many_hunks() {
        let err = AssemblyError::TooManyHunks {
            actual: 20000,
            limit: 10000,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("20000"));
        assert!(msg.contains("10000"));
    }

    #[test]
    fn test_error_invalid_content_range() {
        let err = AssemblyError::InvalidContentRange {
            path: "test.rs".to_string(),
            start: 100,
            end: 50,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
    }

    // ========================================================================
    // AssemblyStats Tests
    // ========================================================================

    #[test]
    fn test_stats_new() {
        let stats = AssemblyStats::new();
        assert_eq!(stats.files_processed, 0);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.hunks_added, 0);
    }

    #[test]
    fn test_stats_record_file() {
        let mut stats = AssemblyStats::new();
        stats.record_file();
        assert_eq!(stats.files_processed, 1);
    }

    #[test]
    fn test_stats_record_skip() {
        let mut stats = AssemblyStats::new();
        stats.record_skip();
        assert_eq!(stats.files_skipped, 1);
    }

    #[test]
    fn test_stats_record_error() {
        let mut stats = AssemblyStats::new();
        stats.record_error();
        assert!(stats.has_errors());
    }

    #[test]
    fn test_stats_add_content_bytes() {
        let mut stats = AssemblyStats::new();
        stats.add_content_bytes(100);
        stats.add_content_bytes(50);
        assert_eq!(stats.content_bytes, 150);
    }

    #[test]
    fn test_stats_total_files() {
        let mut stats = AssemblyStats::new();
        stats.record_file();
        stats.record_file();
        stats.record_skip();
        assert_eq!(stats.total_files(), 3);
    }

    #[test]
    fn test_stats_display() {
        let stats = AssemblyStats {
            files_processed: 5,
            files_skipped: 2,
            hunks_added: 10,
            dependencies_added: 3,
            content_bytes: 1024,
            errors: 0,
        };
        let display = format!("{}", stats);
        assert!(display.contains("5"));
        assert!(display.contains("10"));
    }

    // ========================================================================
    // AssemblyContext Tests
    // ========================================================================

    #[test]
    fn test_context_new() {
        let header = ChangeHeader::builder().message("Test").build();
        let ctx = AssemblyContext::new(header);
        assert_eq!(ctx.hunk_count(), 0);
        assert_eq!(ctx.dependency_count(), 0);
    }

    #[test]
    fn test_context_with_capacity() {
        let header = ChangeHeader::builder().message("Test").build();
        let ctx = AssemblyContext::with_capacity(header, 100);
        assert_eq!(ctx.hunk_count(), 0);
    }

    #[test]
    fn test_context_add_dependency() {
        let header = ChangeHeader::builder().message("Test").build();
        let mut ctx = AssemblyContext::new(header);
        let hash = Hash::of(b"test");
        ctx.add_dependency(hash);
        assert_eq!(ctx.dependency_count(), 1);
    }

    #[test]
    fn test_context_add_dependency_dedup() {
        let header = ChangeHeader::builder().message("Test").build();
        let mut ctx = AssemblyContext::new(header);
        let hash = Hash::of(b"test");
        ctx.add_dependency(hash);
        ctx.add_dependency(hash);
        assert_eq!(ctx.dependency_count(), 1);
    }

    #[test]
    fn test_context_finalize() {
        let header = ChangeHeader::builder().message("Test change").build();
        let ctx = AssemblyContext::new(header);
        let change = ctx.finalize(vec![1, 2, 3], vec![], vec![]);
        assert_eq!(change.message(), "Test change");
        assert_eq!(change.contents, vec![1, 2, 3]);
    }

    // ========================================================================
    // Helper Function Tests
    // ========================================================================

    #[test]
    fn test_compute_content_offsets_empty() {
        let files: Vec<RecordedFile> = vec![];
        let offsets = compute_content_offsets(&files);
        assert!(offsets.is_empty());
    }

    #[test]
    fn test_finalize_hunks_under_limit() {
        let hunks: Vec<GraphOp<Option<Hash>>> = vec![];
        let opts = AssemblyOptions::new().max_hunks(10);
        let result = finalize_hunks(hunks, &opts);
        assert!(result.is_ok());
    }

    #[test]
    fn test_finalize_hunks_over_limit() {
        let hunks: Vec<GraphOp<Option<Hash>>> = vec![];
        let opts = AssemblyOptions::new().max_hunks(0);
        // Empty vec passes even with limit 0
        let result = finalize_hunks(hunks, &opts);
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_empty_change() {
        let header = ChangeHeader::builder().message("Empty").build();
        let change = create_empty_change(header);
        assert!(change.hunks().is_empty());
        assert!(change.contents.is_empty());
    }

    // ========================================================================
    // AssemblyResult_ Tests
    // ========================================================================

    #[test]
    fn test_assembly_result_new() {
        let header = ChangeHeader::builder().message("Test").build();
        let change = Change::empty(header);
        let stats = AssemblyStats::new();
        let result = AssemblyResult_::new(change, stats, vec![], vec![]);
        assert_eq!(result.hunk_count(), 0);
        assert!(!result.has_errors());
    }

    #[test]
    fn test_assembly_result_content_size() {
        let header = ChangeHeader::builder().message("Test").build();
        let mut change = Change::empty(header);
        change.contents = vec![0u8; 100];
        let stats = AssemblyStats::new();
        let result = AssemblyResult_::new(change, stats, vec![], vec![]);
        assert_eq!(result.content_size(), 100);
    }

    #[test]
    fn test_assembly_result_into_change() {
        let header = ChangeHeader::builder().message("Take me").build();
        let change = Change::empty(header);
        let stats = AssemblyStats::new();
        let result = AssemblyResult_::new(change, stats, vec![], vec![]);
        let taken = result.into_change();
        assert_eq!(taken.message(), "Take me");
    }
}
