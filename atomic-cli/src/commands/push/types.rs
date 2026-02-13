#![allow(dead_code)]
//! Types for the push command.
//!
//! This module defines the data structures used throughout the push operation,
//! including representations of changes to be pushed and statistics tracking.

use atomic_core::types::{Hash, Merkle};

// =============================================================================
// PushChange
// =============================================================================

/// A change to be pushed to the remote.
///
/// Contains all the information needed to identify and upload a change,
/// including metadata for display purposes.
///
/// # Example
///
/// ```rust
/// use atomic::commands::push::types::PushChange;
/// use atomic_core::types::{Hash, Merkle};
///
/// let hash = Hash::of(b"test change");
/// let state = Merkle::ZERO;
/// let change = PushChange::new(hash, 1, state)
///     .with_tagged(true)
///     .with_message("Add feature");
///
/// assert_eq!(change.sequence, 1);
/// assert!(change.tagged);
/// assert_eq!(change.message, Some("Add feature".to_string()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushChange {
    /// The change hash (content-addressed identifier).
    pub hash: Hash,

    /// Sequence number in the local stack (0-indexed).
    pub sequence: u64,

    /// The Merkle state after this change was applied.
    pub state: Merkle,

    /// Whether this change is tagged (a named snapshot).
    pub tagged: bool,

    /// The change message (first line of description), if loaded.
    pub message: Option<String>,

    /// Whether the change already exists in the remote graph.
    ///
    /// When `true`, this change is already stored on the server (applied
    /// via another stack) and only needs to be adopted into the target
    /// stack's view — no data transfer is required.
    ///
    /// Stacks are views of the same underlying graph. When pushing to a
    /// new stack, changes from other stacks don't need re-uploading.
    pub in_graph: bool,
}

impl PushChange {
    /// Create a new push change with minimal information.
    ///
    /// # Arguments
    ///
    /// * `hash` - The change's content hash
    /// * `sequence` - The sequence number in the stack
    /// * `state` - The Merkle state after this change
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::types::PushChange;
    /// use atomic_core::types::{Hash, Merkle};
    ///
    /// let hash = Hash::of(b"change content");
    /// let change = PushChange::new(hash, 0, Merkle::ZERO);
    /// assert_eq!(change.sequence, 0);
    /// assert!(!change.tagged);
    /// assert!(change.message.is_none());
    /// ```
    pub fn new(hash: Hash, sequence: u64, state: Merkle) -> Self {
        Self {
            hash,
            sequence,
            state,
            tagged: false,
            message: None,
            in_graph: false,
        }
    }

    /// Set the tagged flag.
    ///
    /// Tagged changes represent named snapshots in the history.
    ///
    /// # Arguments
    ///
    /// * `tagged` - Whether this change is tagged
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::types::PushChange;
    /// use atomic_core::types::{Hash, Merkle};
    ///
    /// let change = PushChange::new(Hash::of(b"x"), 0, Merkle::ZERO)
    ///     .with_tagged(true);
    /// assert!(change.tagged);
    /// ```
    pub fn with_tagged(mut self, tagged: bool) -> Self {
        self.tagged = tagged;
        self
    }

    /// Set the change message.
    ///
    /// The message is typically the first line of the change description,
    /// used for display purposes.
    ///
    /// # Arguments
    ///
    /// * `message` - The change message
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::types::PushChange;
    /// use atomic_core::types::{Hash, Merkle};
    ///
    /// let change = PushChange::new(Hash::of(b"x"), 0, Merkle::ZERO)
    ///     .with_message("Fix authentication bug");
    /// assert_eq!(change.message.as_deref(), Some("Fix authentication bug"));
    /// ```
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Check if this change has a message.
    pub fn has_message(&self) -> bool {
        self.message.is_some()
    }

    /// Get the message or a default placeholder.
    pub fn message_or_default(&self) -> &str {
        self.message.as_deref().unwrap_or("(no message)")
    }

    /// Mark this change as already existing in the remote graph.
    ///
    /// Changes that are already in the graph only need stack adoption,
    /// not a full data upload. This happens when pushing to a new stack
    /// where the changes were already pushed via another stack.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::types::PushChange;
    /// use atomic_core::types::{Hash, Merkle};
    ///
    /// let change = PushChange::new(Hash::of(b"x"), 0, Merkle::ZERO)
    ///     .with_in_graph(true);
    /// assert!(change.in_graph);
    /// ```
    pub fn with_in_graph(mut self, in_graph: bool) -> Self {
        self.in_graph = in_graph;
        self
    }

    /// Check if this change needs a full data upload.
    ///
    /// Returns `true` if the change is NOT already in the remote graph
    /// and requires transferring the change data over the network.
    pub fn needs_upload(&self) -> bool {
        !self.in_graph
    }
}

// =============================================================================
// PushStats
// =============================================================================

/// Statistics about a push operation.
///
/// Tracks metrics about what was pushed, useful for reporting and testing.
///
/// # Example
///
/// ```rust
/// use atomic::commands::push::types::PushStats;
///
/// let mut stats = PushStats::new();
/// assert_eq!(stats.changes_uploaded, 0);
/// assert_eq!(stats.total_uploaded(), 0);
///
/// stats.changes_uploaded = 5;
/// stats.tags_uploaded = 2;
/// assert_eq!(stats.total_uploaded(), 7);
/// assert!(stats.has_uploads());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushStats {
    /// Number of changes successfully uploaded.
    pub changes_uploaded: usize,

    /// Number of tags successfully uploaded.
    pub tags_uploaded: usize,

    /// Total bytes transferred to the remote.
    pub bytes_transferred: u64,

    /// Number of changes skipped (already on remote).
    pub changes_skipped: usize,

    /// Number of changes that failed to upload.
    pub changes_failed: usize,
}

impl PushStats {
    /// Create new empty statistics.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::types::PushStats;
    ///
    /// let stats = PushStats::new();
    /// assert_eq!(stats.changes_uploaded, 0);
    /// assert_eq!(stats.bytes_transferred, 0);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the total number of items uploaded (changes + tags).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::types::PushStats;
    ///
    /// let mut stats = PushStats::new();
    /// stats.changes_uploaded = 3;
    /// stats.tags_uploaded = 1;
    /// assert_eq!(stats.total_uploaded(), 4);
    /// ```
    pub fn total_uploaded(&self) -> usize {
        self.changes_uploaded + self.tags_uploaded
    }

    /// Check if anything was uploaded.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::types::PushStats;
    ///
    /// let stats = PushStats::new();
    /// assert!(!stats.has_uploads());
    /// ```
    pub fn has_uploads(&self) -> bool {
        self.total_uploaded() > 0
    }

    /// Check if the operation was a no-op (nothing to push).
    ///
    /// Returns true if nothing was uploaded but some changes were skipped,
    /// indicating the remote was already up to date.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::push::types::PushStats;
    ///
    /// let mut stats = PushStats::new();
    /// stats.changes_skipped = 5;
    /// assert!(stats.is_noop());
    /// ```
    pub fn is_noop(&self) -> bool {
        self.total_uploaded() == 0 && self.changes_skipped > 0
    }

    /// Check if there were any failures.
    pub fn has_failures(&self) -> bool {
        self.changes_failed > 0
    }

    /// Record a successful change upload.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Number of bytes transferred for this change
    pub fn record_change_uploaded(&mut self, bytes: u64) {
        self.changes_uploaded += 1;
        self.bytes_transferred += bytes;
    }

    /// Record a successful tag upload.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Number of bytes transferred for this tag
    pub fn record_tag_uploaded(&mut self, bytes: u64) {
        self.tags_uploaded += 1;
        self.bytes_transferred += bytes;
    }

    /// Record a skipped change.
    pub fn record_skipped(&mut self) {
        self.changes_skipped += 1;
    }

    /// Record a failed change.
    pub fn record_failed(&mut self) {
        self.changes_failed += 1;
    }
}

// =============================================================================
// PushOutcome
// =============================================================================

/// The outcome of a push operation.
///
/// Contains both the statistics and any relevant state information
/// about the completed push.
#[derive(Debug, Clone)]
pub struct PushOutcome {
    /// Statistics about the push operation.
    pub stats: PushStats,

    /// The final Merkle state on the remote after pushing.
    pub remote_state: Option<Merkle>,

    /// Whether the push was a dry run (no actual changes made).
    pub dry_run: bool,

    /// Any warning messages generated during the push.
    pub warnings: Vec<String>,
}

impl PushOutcome {
    /// Create a new push outcome.
    pub fn new(stats: PushStats) -> Self {
        Self {
            stats,
            remote_state: None,
            dry_run: false,
            warnings: Vec::new(),
        }
    }

    /// Create a dry run outcome.
    pub fn dry_run(stats: PushStats) -> Self {
        Self {
            stats,
            remote_state: None,
            dry_run: true,
            warnings: Vec::new(),
        }
    }

    /// Set the remote state.
    pub fn with_remote_state(mut self, state: Merkle) -> Self {
        self.remote_state = Some(state);
        self
    }

    /// Add a warning message.
    pub fn add_warning(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    /// Check if the push was successful (no failures).
    pub fn is_success(&self) -> bool {
        !self.stats.has_failures()
    }

    /// Check if there are any warnings.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

impl Default for PushOutcome {
    fn default() -> Self {
        Self::new(PushStats::new())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // PushChange Tests
    // =========================================================================

    #[test]
    fn test_push_change_new() {
        let hash = Hash::of(b"test");
        let state = Merkle::ZERO;
        let change = PushChange::new(hash, 42, state);

        assert_eq!(change.hash, hash);
        assert_eq!(change.sequence, 42);
        assert_eq!(change.state, state);
        assert!(!change.tagged);
        assert!(change.message.is_none());
    }

    #[test]
    fn test_push_change_with_tagged() {
        let hash = Hash::of(b"test");
        let change = PushChange::new(hash, 0, Merkle::ZERO).with_tagged(true);
        assert!(change.tagged);

        let change2 = change.clone().with_tagged(false);
        assert!(!change2.tagged);
    }

    #[test]
    fn test_push_change_with_message() {
        let hash = Hash::of(b"test");
        let change = PushChange::new(hash, 0, Merkle::ZERO).with_message("Add feature");

        assert_eq!(change.message, Some("Add feature".to_string()));
        assert!(change.has_message());
        assert_eq!(change.message_or_default(), "Add feature");
    }

    #[test]
    fn test_push_change_message_or_default() {
        let hash = Hash::of(b"test");
        let change = PushChange::new(hash, 0, Merkle::ZERO);

        assert!(!change.has_message());
        assert_eq!(change.message_or_default(), "(no message)");
    }

    #[test]
    fn test_push_change_builder_chain() {
        let hash = Hash::of(b"test");
        let change = PushChange::new(hash, 5, Merkle::ZERO)
            .with_tagged(true)
            .with_message("Fix bug");

        assert_eq!(change.sequence, 5);
        assert!(change.tagged);
        assert_eq!(change.message.as_deref(), Some("Fix bug"));
    }

    #[test]
    fn test_push_change_equality() {
        let hash = Hash::of(b"test");
        let change1 = PushChange::new(hash, 0, Merkle::ZERO);
        let change2 = PushChange::new(hash, 0, Merkle::ZERO);
        assert_eq!(change1, change2);

        let change3 = PushChange::new(hash, 1, Merkle::ZERO);
        assert_ne!(change1, change3);
    }

    #[test]
    fn test_push_change_clone() {
        let hash = Hash::of(b"test");
        let change = PushChange::new(hash, 0, Merkle::ZERO)
            .with_tagged(true)
            .with_message("msg");
        let cloned = change.clone();

        assert_eq!(change, cloned);
    }

    #[test]
    fn test_push_change_debug() {
        let hash = Hash::of(b"test");
        let change = PushChange::new(hash, 0, Merkle::ZERO);
        let debug_str = format!("{:?}", change);

        assert!(debug_str.contains("PushChange"));
        assert!(debug_str.contains("sequence: 0"));
    }

    // =========================================================================
    // PushStats Tests
    // =========================================================================

    #[test]
    fn test_push_stats_new() {
        let stats = PushStats::new();

        assert_eq!(stats.changes_uploaded, 0);
        assert_eq!(stats.tags_uploaded, 0);
        assert_eq!(stats.bytes_transferred, 0);
        assert_eq!(stats.changes_skipped, 0);
        assert_eq!(stats.changes_failed, 0);
    }

    #[test]
    fn test_push_stats_default() {
        let stats = PushStats::default();
        assert_eq!(stats, PushStats::new());
    }

    #[test]
    fn test_push_stats_total_uploaded() {
        let mut stats = PushStats::new();
        assert_eq!(stats.total_uploaded(), 0);

        stats.changes_uploaded = 3;
        assert_eq!(stats.total_uploaded(), 3);

        stats.tags_uploaded = 2;
        assert_eq!(stats.total_uploaded(), 5);
    }

    #[test]
    fn test_push_stats_has_uploads() {
        let mut stats = PushStats::new();
        assert!(!stats.has_uploads());

        stats.changes_uploaded = 1;
        assert!(stats.has_uploads());
    }

    #[test]
    fn test_push_stats_is_noop() {
        let mut stats = PushStats::new();
        assert!(!stats.is_noop()); // Nothing happened at all

        stats.changes_skipped = 5;
        assert!(stats.is_noop()); // Skipped but nothing uploaded

        stats.changes_uploaded = 1;
        assert!(!stats.is_noop()); // Uploaded something
    }

    #[test]
    fn test_push_stats_has_failures() {
        let mut stats = PushStats::new();
        assert!(!stats.has_failures());

        stats.changes_failed = 1;
        assert!(stats.has_failures());
    }

    #[test]
    fn test_push_stats_record_change_uploaded() {
        let mut stats = PushStats::new();
        stats.record_change_uploaded(1024);

        assert_eq!(stats.changes_uploaded, 1);
        assert_eq!(stats.bytes_transferred, 1024);

        stats.record_change_uploaded(512);
        assert_eq!(stats.changes_uploaded, 2);
        assert_eq!(stats.bytes_transferred, 1536);
    }

    #[test]
    fn test_push_stats_record_tag_uploaded() {
        let mut stats = PushStats::new();
        stats.record_tag_uploaded(256);

        assert_eq!(stats.tags_uploaded, 1);
        assert_eq!(stats.bytes_transferred, 256);
    }

    #[test]
    fn test_push_stats_record_skipped() {
        let mut stats = PushStats::new();
        stats.record_skipped();
        stats.record_skipped();

        assert_eq!(stats.changes_skipped, 2);
    }

    #[test]
    fn test_push_stats_record_failed() {
        let mut stats = PushStats::new();
        stats.record_failed();

        assert_eq!(stats.changes_failed, 1);
        assert!(stats.has_failures());
    }

    #[test]
    fn test_push_stats_equality() {
        let stats1 = PushStats::new();
        let stats2 = PushStats::new();
        assert_eq!(stats1, stats2);

        let mut stats3 = PushStats::new();
        stats3.changes_uploaded = 1;
        assert_ne!(stats1, stats3);
    }

    #[test]
    fn test_push_stats_clone() {
        let mut stats = PushStats::new();
        stats.changes_uploaded = 5;
        stats.bytes_transferred = 10000;

        let cloned = stats.clone();
        assert_eq!(stats, cloned);
    }

    // =========================================================================
    // PushOutcome Tests
    // =========================================================================

    #[test]
    fn test_push_outcome_new() {
        let stats = PushStats::new();
        let outcome = PushOutcome::new(stats);

        assert!(!outcome.dry_run);
        assert!(outcome.remote_state.is_none());
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn test_push_outcome_dry_run() {
        let stats = PushStats::new();
        let outcome = PushOutcome::dry_run(stats);

        assert!(outcome.dry_run);
    }

    #[test]
    fn test_push_outcome_with_remote_state() {
        let stats = PushStats::new();
        let state = Hash::of(b"state").into();
        let outcome = PushOutcome::new(stats).with_remote_state(state);

        assert_eq!(outcome.remote_state, Some(state));
    }

    #[test]
    fn test_push_outcome_add_warning() {
        let stats = PushStats::new();
        let mut outcome = PushOutcome::new(stats);

        assert!(!outcome.has_warnings());

        outcome.add_warning("Something might be wrong");
        assert!(outcome.has_warnings());
        assert_eq!(outcome.warnings.len(), 1);
        assert_eq!(outcome.warnings[0], "Something might be wrong");
    }

    #[test]
    fn test_push_outcome_is_success() {
        let stats = PushStats::new();
        let outcome = PushOutcome::new(stats);
        assert!(outcome.is_success());

        let mut stats_with_failure = PushStats::new();
        stats_with_failure.changes_failed = 1;
        let outcome_with_failure = PushOutcome::new(stats_with_failure);
        assert!(!outcome_with_failure.is_success());
    }

    #[test]
    fn test_push_outcome_default() {
        let outcome = PushOutcome::default();
        assert!(!outcome.dry_run);
        assert!(outcome.is_success());
    }
}
