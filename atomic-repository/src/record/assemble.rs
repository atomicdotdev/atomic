//! Record statistics, outcomes, and helper functions.
//!
//! This module contains the output types produced by the recording pipeline:
//! [`RecordStats`] for tracking recording metrics and [`RecordOutcome`] for
//! the complete result of a record operation. Also includes helper functions
//! for building headers and filtering files.

use std::fmt;

use atomic_core::change::{Change, ChangeHeader};
use atomic_core::types::{Base32, Hash, Merkle};

use crate::status::{FileStatus, FileStatusEntry};

use super::options::RecordOptions;

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

    /// Number of vault files deflated (synced from disk to redb).
    pub vault_files_deflated: usize,
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

    /// Vault paths that were deflated (synced from disk to redb).
    vault_paths: Vec<String>,

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
            vault_paths: Vec::new(),
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

    /// Add an error.
    pub fn add_error(&mut self, path: String, error: String) {
        self.errors.push((path, error));
    }

    /// Set the vault paths that were deflated during this record.
    pub fn set_vault_paths(&mut self, paths: Vec<String>) {
        self.vault_paths = paths;
    }

    /// Get the vault paths that were deflated.
    pub fn vault_paths(&self) -> &[String] {
        &self.vault_paths
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
