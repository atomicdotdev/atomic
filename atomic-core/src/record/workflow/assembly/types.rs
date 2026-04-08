//! Types for change assembly: errors, options, and results.

use thiserror::Error;

use crate::change::{Change, Provenance};

use super::super::globalize::{GlobalizeError, GlobalizeOptions, GlobalizedFile};
use super::helpers::AssemblyStats;

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
