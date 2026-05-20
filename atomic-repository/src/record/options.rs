//! Record options and configuration types.
//!
//! This module contains the [`RecordOptions`] builder for controlling
//! which files are recorded and how they are processed.

use atomic_core::change::{Encoding, Provenance};
use atomic_core::diff::Algorithm;
use atomic_core::record::workflow::{
    AssemblyOptions, GlobalizeOptions, RecordingOptions as CoreRecordingOptions,
};

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

    /// View to record to (None = current view).
    view: Option<String>,

    /// Change message (can also be set in header).
    message: Option<String>,

    /// Whether to apply the change after recording.
    apply_after_record: bool,

    /// Whether to save the change to the store.
    save_to_store: bool,

    /// Whether to refresh the file index after applying the recorded change.
    update_file_index: bool,

    /// Whether to deflate vault working-copy files after recording.
    sync_vault: bool,

    /// Whether to enrich the knowledge graph for the newly recorded change.
    enrich_kg: bool,

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
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set specific paths to record.
    ///
    /// When paths are specified, only changes in those files will be recorded.
    /// Other modified files will be ignored.
    #[must_use]
    pub fn paths(mut self, paths: Vec<impl Into<String>>) -> Self {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Add a single path to record.
    #[must_use]
    pub fn add_path(mut self, path: impl Into<String>) -> Self {
        self.paths.push(path.into());
        self
    }

    /// Set whether to record all changes.
    ///
    /// When true, ignores the paths filter and records all modified files.
    #[must_use]
    pub fn with_all(mut self, all: bool) -> Self {
        self.all = all;
        self
    }

    /// Set the diff algorithm.
    #[must_use]
    pub fn with_algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set the default encoding for new files.
    #[must_use]
    pub fn with_default_encoding(mut self, encoding: Encoding) -> Self {
        self.default_encoding = encoding;
        self
    }

    /// Set maximum file size for diffing. Files larger than this are treated as binary.
    #[must_use]
    pub fn with_max_file_size(mut self, size: u64) -> Self {
        self.max_file_size = size;
        self
    }

    /// Set whether to skip binary files.
    #[must_use]
    pub fn with_skip_binary(mut self, skip: bool) -> Self {
        self.skip_binary = skip;
        self
    }

    /// Set whether to record empty files.
    #[must_use]
    pub fn record_empty_files(mut self, record: bool) -> Self {
        self.record_empty_files = record;
        self
    }

    /// Set the number of context lines.
    #[must_use]
    pub fn context_lines(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Set the target view. If not set, uses the current view.
    #[must_use]
    pub fn view(mut self, view: impl Into<String>) -> Self {
        self.view = Some(view.into());
        self
    }

    /// Set the change message.
    #[must_use]
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Set whether to apply the change after recording. Default is true.
    #[must_use]
    pub fn apply_after_record(mut self, apply: bool) -> Self {
        self.apply_after_record = apply;
        self
    }

    /// Set whether to save the change to the store. Default is true.
    #[must_use]
    pub fn save_to_store(mut self, save: bool) -> Self {
        self.save_to_store = save;
        self
    }

    /// Set whether to refresh the file index after applying the recorded change.
    ///
    /// Defaults to true for normal user records. Agent hooks may disable this
    /// to keep turn-end latency low.
    #[must_use]
    pub fn update_file_index(mut self, update: bool) -> Self {
        self.update_file_index = update;
        self
    }

    /// Set whether to deflate vault working-copy files after recording.
    ///
    /// Defaults to true for normal user records. Agent hooks may disable this
    /// because vault sync is best-effort maintenance, not required for the
    /// recorded change itself.
    #[must_use]
    pub fn sync_vault(mut self, sync: bool) -> Self {
        self.sync_vault = sync;
        self
    }

    /// Set whether to enrich the knowledge graph after recording.
    ///
    /// Defaults to true for normal user records. Agent hooks may disable this
    /// so enrichment can happen explicitly instead of blocking Stop hooks.
    #[must_use]
    pub fn enrich_kg(mut self, enrich: bool) -> Self {
        self.enrich_kg = enrich;
        self
    }

    /// Set opaque metadata bytes for `HashedChange.metadata`.
    ///
    /// These bytes are included in the change's cryptographic hash,
    /// making them tamper-evident. Used by `atomic-agent` to embed
    /// the `SessionEnvelope` (turn number, session ID, timing, files).
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
    #[must_use]
    pub fn provenance(mut self, provenance: Vec<Provenance>) -> Self {
        self.provenance = provenance;
        self
    }

    /// Add a single provenance entry.
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

    /// Get the target view.
    #[must_use]
    pub fn get_view(&self) -> Option<&str> {
        self.view.as_deref()
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

    /// Get whether to refresh the file index after applying.
    #[must_use]
    pub fn get_update_file_index(&self) -> bool {
        self.update_file_index
    }

    /// Get whether to deflate vault working-copy files after recording.
    #[must_use]
    pub fn get_sync_vault(&self) -> bool {
        self.sync_vault
    }

    /// Get whether to enrich the knowledge graph after recording.
    #[must_use]
    pub fn get_enrich_kg(&self) -> bool {
        self.enrich_kg
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

    /// Check if a path should be included based on the configured paths filter.
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
            record_empty_files: true,
            context_lines: Self::DEFAULT_CONTEXT_LINES,
            view: None,
            message: None,
            apply_after_record: true,
            save_to_store: true,
            update_file_index: true,
            sync_vault: true,
            enrich_kg: true,
            provenance: Vec::new(),
        }
    }
}
