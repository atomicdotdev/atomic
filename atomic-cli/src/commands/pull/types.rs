//! Types for the pull command.
//!
//! This module defines the data structures used throughout the pull operation,
//! including representations of changes to be downloaded and statistics tracking.
//!
//! # Overview
//!
//! The pull command uses three main types:
//!
//! - [`PullChange`]: Represents a single change to be downloaded from the remote
//! - [`PullStats`]: Tracks statistics about the pull operation (downloads, failures, etc.)
//! - [`PullOutcome`]: The final result of a pull operation
//!
//! # Design Philosophy
//!
//! These types are designed to be:
//!
//! 1. **Immutable by default**: Use builder methods to construct instances
//! 2. **Testable**: All types implement common traits like `Debug`, `Clone`, `PartialEq`
//! 3. **Self-documenting**: Rich documentation with examples
//! 4. **Type-safe**: Leverage Rust's type system to prevent invalid states

use atomic_core::types::{Hash, Merkle};

// PullChange

/// A change to be downloaded from the remote.
///
/// Contains all the information needed to identify and download a change,
/// including metadata for display purposes.
///
/// # Example
///
/// ```rust
/// use atomic::commands::pull::types::PullChange;
/// use atomic_core::types::{Hash, Merkle};
///
/// let hash = Hash::of(b"test change");
/// let state = Merkle::ZERO;
/// let change = PullChange::new(hash, 1, state)
///     .with_tagged(true)
///     .with_message("Add feature");
///
/// assert_eq!(change.sequence, 1);
/// assert!(change.tagged);
/// assert_eq!(change.message, Some("Add feature".to_string()));
/// ```
///
/// # Fields
///
/// - `hash`: The content-addressed identifier for this change
/// - `sequence`: The position in the remote's change log
/// - `state`: The Merkle state after this change was applied
/// - `tagged`: Whether this change represents a named snapshot
/// - `message`: Optional commit message for display purposes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullChange {
    /// The change hash (content-addressed identifier).
    ///
    /// This is the Blake3 hash of the change content, used to uniquely
    /// identify the change across all repositories.
    pub hash: Hash,

    /// Sequence number in the remote stack (0-indexed).
    ///
    /// This indicates the order of this change in the remote's history.
    /// Changes must be applied in sequence order to maintain consistency.
    pub sequence: u64,

    /// The Merkle state after this change was applied.
    ///
    /// This represents the cumulative state of the stack after applying
    /// this change, enabling efficient state comparison.
    pub state: Merkle,

    /// Whether this change is tagged (a named snapshot).
    ///
    /// Tagged changes have associated tag files that need to be downloaded
    /// separately from the change itself.
    pub tagged: bool,

    /// The change message (first line of description), if known.
    ///
    /// This is typically loaded from the change header after download,
    /// but may be provided during delta calculation if the change
    /// already exists locally.
    pub message: Option<String>,
}

impl PullChange {
    /// Create a new pull change with minimal information.
    ///
    /// Creates a `PullChange` with the required fields and sensible defaults
    /// for optional fields (`tagged = false`, `message = None`).
    ///
    /// # Arguments
    ///
    /// * `hash` - The change's content hash
    /// * `sequence` - The sequence number in the remote stack
    /// * `state` - The Merkle state after this change
    ///
    /// # Returns
    ///
    /// A new `PullChange` instance.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullChange;
    /// use atomic_core::types::{Hash, Merkle};
    ///
    /// let hash = Hash::of(b"change content");
    /// let change = PullChange::new(hash, 0, Merkle::ZERO);
    ///
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
        }
    }

    /// Set the tagged flag.
    ///
    /// Tagged changes represent named snapshots in the history. When a change
    /// is tagged, the pull operation will also download the associated tag file.
    ///
    /// # Arguments
    ///
    /// * `tagged` - Whether this change is tagged
    ///
    /// # Returns
    ///
    /// Self with the tagged flag set, enabling method chaining.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullChange;
    /// use atomic_core::types::{Hash, Merkle};
    ///
    /// let change = PullChange::new(Hash::of(b"x"), 0, Merkle::ZERO)
    ///     .with_tagged(true);
    ///
    /// assert!(change.tagged);
    /// ```
    pub fn with_tagged(mut self, tagged: bool) -> Self {
        self.tagged = tagged;
        self
    }

    /// Builder: set the message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Check if this change is tagged.
    pub fn is_tagged(&self) -> bool {
        self.tagged
    }

    /// Check if this change has a message.
    pub fn has_message(&self) -> bool {
        self.message.is_some()
    }

    /// message_or_default
    /// Get the message or a default placeholder.
    ///
    /// Returns the change message if one is set, or "(no message)" as a
    /// fallback for display purposes.
    ///
    /// # Returns
    ///
    /// The message string, or "(no message)" if not set.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullChange;
    /// use atomic_core::types::{Hash, Merkle};
    ///
    /// let change = PullChange::new(Hash::of(b"x"), 0, Merkle::ZERO);
    /// assert_eq!(change.message_or_default(), "(no message)");
    ///
    /// let change = change.with_message("My commit");
    /// assert_eq!(change.message_or_default(), "My commit");
    /// ```
    pub fn message_or_default(&self) -> &str {
        self.message.as_deref().unwrap_or("(no message)")
    }
}

// PullStats

/// Statistics about a pull operation.
///
/// Tracks metrics about what was pulled, useful for reporting to the user
/// and for testing assertions.
///
/// # Fields
///
/// - `changes_downloaded`: Number of change files successfully downloaded
/// - `tags_downloaded`: Number of tag files successfully downloaded
/// - `bytes_transferred`: Total bytes received from the remote
/// - `changes_skipped`: Number of changes that already existed locally
/// - `changes_failed`: Number of changes that failed to download
/// - `changes_applied`: Number of changes successfully applied to the stack
///
/// # Example
///
/// ```rust
/// use atomic::commands::pull::types::PullStats;
///
/// let mut stats = PullStats::new();
/// assert_eq!(stats.changes_downloaded, 0);
/// assert_eq!(stats.total_downloaded(), 0);
///
/// stats.changes_downloaded = 5;
/// stats.tags_downloaded = 2;
/// assert_eq!(stats.total_downloaded(), 7);
/// assert!(stats.has_downloads());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullStats {
    /// Number of changes successfully downloaded.
    ///
    /// Incremented each time a change file is successfully received from
    /// the remote and saved to the local change store.
    pub changes_downloaded: usize,

    /// Number of tags successfully downloaded.
    ///
    /// Incremented each time a tag file is successfully received and saved.
    pub tags_downloaded: usize,

    /// Total bytes transferred from the remote.
    ///
    /// The cumulative size of all downloaded change and tag files,
    /// useful for bandwidth reporting.
    pub bytes_transferred: u64,

    /// Number of changes skipped (already exist locally).
    ///
    /// When a change already exists in the local change store, it doesn't
    /// need to be downloaded again.
    pub changes_skipped: usize,

    /// Number of changes that failed to download.
    ///
    /// Counts download failures due to network errors, authentication
    /// issues, or other problems.
    pub changes_failed: usize,

    /// Number of changes successfully applied to the local stack.
    ///
    /// This may differ from `changes_downloaded` if `--download-only` is used,
    /// or if some changes fail to apply due to conflicts.
    pub changes_applied: usize,
}

impl PullStats {
    /// Create new empty statistics.
    ///
    /// All counters are initialized to zero.
    ///
    /// # Returns
    ///
    /// A new `PullStats` instance with all fields set to zero.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullStats;
    ///
    /// let stats = PullStats::new();
    /// assert_eq!(stats.changes_downloaded, 0);
    /// assert_eq!(stats.bytes_transferred, 0);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any downloads failed.
    ///
    /// # Returns
    ///
    /// `true` if at least one change failed to download.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullStats;
    ///
    /// let mut stats = PullStats::new();
    /// assert!(!stats.has_failures());
    ///
    /// stats.changes_failed = 1;
    /// assert!(stats.has_failures());
    /// ```
    pub fn has_failures(&self) -> bool {
        self.changes_failed > 0
    }

    /// Check if any changes were applied to the stack.
    ///
    /// # Returns
    ///
    /// `true` if at least one change was applied.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullStats;
    ///
    /// let mut stats = PullStats::new();
    /// assert!(!stats.has_applied());
    ///
    /// stats.changes_applied = 3;
    /// assert!(stats.has_applied());
    /// ```
    pub fn has_applied(&self) -> bool {
        self.changes_applied > 0
    }

    /// Record a successfully downloaded change.
    ///
    /// Increments the download counter and adds the bytes to the transfer total.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The size of the downloaded change file in bytes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullStats;
    ///
    /// let mut stats = PullStats::new();
    /// stats.record_change_downloaded(1024);
    /// stats.record_change_downloaded(2048);
    ///
    /// assert_eq!(stats.changes_downloaded, 2);
    /// assert_eq!(stats.bytes_transferred, 3072);
    /// ```
    pub fn record_change_downloaded(&mut self, bytes: u64) {
        self.changes_downloaded += 1;
        self.bytes_transferred += bytes;
    }

    /// Record a failed download.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullStats;
    ///
    /// let mut stats = PullStats::new();
    /// stats.record_failed();
    ///
    /// assert_eq!(stats.changes_failed, 1);
    /// assert!(stats.has_failures());
    /// ```
    pub fn record_failed(&mut self) {
        self.changes_failed += 1;
    }

    /// Record a successfully applied change.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::pull::types::PullStats;
    ///
    /// let mut stats = PullStats::new();
    /// stats.record_applied();
    /// stats.record_applied();
    ///
    /// assert_eq!(stats.changes_applied, 2);
    /// assert!(stats.has_applied());
    /// ```
    pub fn record_applied(&mut self) {
        self.changes_applied += 1;
    }

    /// Get total number of items downloaded (changes + tags).
    pub fn total_downloaded(&self) -> usize {
        self.changes_downloaded + self.tags_downloaded
    }

    /// Check if any items were downloaded.
    pub fn has_downloads(&self) -> bool {
        self.total_downloaded() > 0
    }

    /// Check if the pull was a no-op (nothing downloaded, nothing applied, nothing skipped).
    ///
    /// Returns `false` if any work was done, including skipping changes
    /// that already exist locally (since that means the remote had changes).
    pub fn is_noop(&self) -> bool {
        self.changes_downloaded == 0
            && self.tags_downloaded == 0
            && self.changes_applied == 0
            && self.changes_failed == 0
            && self.changes_skipped == 0
    }

    /// Record a successfully downloaded tag.
    pub fn record_tag_downloaded(&mut self, bytes: u64) {
        self.tags_downloaded += 1;
        self.bytes_transferred += bytes;
    }

    /// Record a skipped change (already exists locally).
    pub fn record_skipped(&mut self) {
        self.changes_skipped += 1;
    }

    /// Merge another stats instance into this one.
    pub fn merge(&mut self, other: &PullStats) {
        self.changes_downloaded += other.changes_downloaded;
        self.tags_downloaded += other.tags_downloaded;
        self.bytes_transferred += other.bytes_transferred;
        self.changes_skipped += other.changes_skipped;
        self.changes_failed += other.changes_failed;
        self.changes_applied += other.changes_applied;
    }
}

// PullOutcome

/// The result of a pull operation.
///
/// Contains the final statistics, state information, and any warnings
/// that occurred during the pull.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PullOutcome {
    /// Pull statistics.
    pub stats: PullStats,
    /// Whether this was a dry-run pull (no changes applied).
    pub dry_run: bool,
    /// Whether this was a download-only pull (no apply).
    pub download_only: bool,
    /// The remote Merkle state after pulling.
    pub remote_state: Option<Merkle>,
    /// The local Merkle state after pulling.
    pub local_state: Option<Merkle>,
    /// Any warnings produced during the pull.
    pub warnings: Vec<String>,
    /// Changes that exist locally but not on the remote.
    pub local_only_changes: Vec<String>,
}

impl PullOutcome {
    /// Create a new pull outcome with the given stats.
    pub fn new(stats: PullStats) -> Self {
        Self {
            stats,
            dry_run: false,
            download_only: false,
            remote_state: None,
            local_state: None,
            warnings: Vec::new(),
            local_only_changes: Vec::new(),
        }
    }

    /// Create a dry-run pull outcome.
    pub fn dry_run(stats: PullStats) -> Self {
        Self {
            stats,
            dry_run: true,
            download_only: false,
            remote_state: None,
            local_state: None,
            warnings: Vec::new(),
            local_only_changes: Vec::new(),
        }
    }

    /// Create a download-only pull outcome.
    pub fn download_only(stats: PullStats) -> Self {
        Self {
            stats,
            dry_run: false,
            download_only: true,
            remote_state: None,
            local_state: None,
            warnings: Vec::new(),
            local_only_changes: Vec::new(),
        }
    }

    /// Builder: set the remote state.
    pub fn with_remote_state(mut self, state: Merkle) -> Self {
        self.remote_state = Some(state);
        self
    }

    /// Builder: set the local state.
    pub fn with_local_state(mut self, state: Merkle) -> Self {
        self.local_state = Some(state);
        self
    }

    /// Add a warning message.
    pub fn add_warning(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    /// Add a local-only change hash.
    pub fn add_local_only_change(&mut self, hash: impl Into<String>) {
        self.local_only_changes.push(hash.into());
    }

    /// Check if the pull was successful (no failures).
    pub fn is_success(&self) -> bool {
        !self.stats.has_failures()
    }

    /// Check if there are warnings.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Check if there are local-only changes.
    pub fn has_local_only_changes(&self) -> bool {
        !self.local_only_changes.is_empty()
    }

    /// Get the number of local-only changes.
    pub fn local_only_count(&self) -> usize {
        self.local_only_changes.len()
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // PullChange Tests

    /// Test creating a PullChange with minimal information.
    #[test]
    fn test_pull_change_new() {
        let hash = Hash::of(b"test content");
        let state = Merkle::ZERO;
        let change = PullChange::new(hash, 42, state);

        assert_eq!(change.hash, hash);
        assert_eq!(change.sequence, 42);
        assert_eq!(change.state, state);
        assert!(!change.tagged);
        assert!(change.message.is_none());
    }

    /// Test setting the tagged flag.
    #[test]
    fn test_pull_change_with_tagged() {
        let change = PullChange::new(Hash::of(b"x"), 0, Merkle::ZERO).with_tagged(true);

        assert!(change.tagged);
        assert!(change.is_tagged());
    }

    /// Test setting the change message.
    #[test]
    fn test_pull_change_with_message() {
        let change =
            PullChange::new(Hash::of(b"x"), 0, Merkle::ZERO).with_message("Fix critical bug");

        assert!(change.has_message());
        assert_eq!(change.message.as_deref(), Some("Fix critical bug"));
    }

    /// Test the message_or_default method.
    #[test]
    fn test_pull_change_message_or_default() {
        let without_msg = PullChange::new(Hash::of(b"x"), 0, Merkle::ZERO);
        assert_eq!(without_msg.message_or_default(), "(no message)");

        let with_msg = without_msg.with_message("Test message");
        assert_eq!(with_msg.message_or_default(), "Test message");
    }

    /// Test chaining multiple builder methods.
    #[test]
    fn test_pull_change_builder_chain() {
        let hash = Hash::of(b"chained");
        let state = Merkle::of(b"state");
        let change = PullChange::new(hash, 100, state)
            .with_tagged(true)
            .with_message("Chained message");

        assert_eq!(change.hash, hash);
        assert_eq!(change.sequence, 100);
        assert_eq!(change.state, state);
        assert!(change.tagged);
        assert!(change.is_tagged());
        assert!(change.has_message());
        assert_eq!(change.message_or_default(), "Chained message");
    }

    /// Test PullChange equality.
    #[test]
    fn test_pull_change_equality() {
        let hash = Hash::of(b"equal");
        let state = Merkle::ZERO;
        let change1 = PullChange::new(hash, 0, state);
        let change2 = PullChange::new(hash, 0, state);

        assert_eq!(change1, change2);
    }

    /// Test PullChange clone.
    #[test]
    fn test_pull_change_clone() {
        let original = PullChange::new(Hash::of(b"clone"), 5, Merkle::ZERO)
            .with_tagged(true)
            .with_message("Clone me");

        let cloned = original.clone();

        assert_eq!(original, cloned);
        assert_eq!(cloned.message, Some("Clone me".to_string()));
    }

    /// Test PullChange debug formatting.
    #[test]
    fn test_pull_change_debug() {
        let change = PullChange::new(Hash::of(b"debug"), 0, Merkle::ZERO);
        let debug_str = format!("{:?}", change);

        assert!(debug_str.contains("PullChange"));
        assert!(debug_str.contains("sequence: 0"));
    }

    // PullStats Tests

    /// Test creating empty statistics.
    #[test]
    fn test_pull_stats_new() {
        let stats = PullStats::new();

        assert_eq!(stats.changes_downloaded, 0);
        assert_eq!(stats.tags_downloaded, 0);
        assert_eq!(stats.bytes_transferred, 0);
        assert_eq!(stats.changes_skipped, 0);
        assert_eq!(stats.changes_failed, 0);
        assert_eq!(stats.changes_applied, 0);
    }

    /// Test Default implementation for PullStats.
    #[test]
    fn test_pull_stats_default() {
        let stats: PullStats = Default::default();
        assert!(stats.is_noop());
    }

    /// Test total_downloaded calculation.
    #[test]
    fn test_pull_stats_total_downloaded() {
        let mut stats = PullStats::new();
        assert_eq!(stats.total_downloaded(), 0);

        stats.changes_downloaded = 10;
        assert_eq!(stats.total_downloaded(), 10);

        stats.tags_downloaded = 5;
        assert_eq!(stats.total_downloaded(), 15);
    }

    /// Test has_downloads check.
    #[test]
    fn test_pull_stats_has_downloads() {
        let mut stats = PullStats::new();
        assert!(!stats.has_downloads());

        stats.changes_downloaded = 1;
        assert!(stats.has_downloads());

        let mut stats2 = PullStats::new();
        stats2.tags_downloaded = 1;
        assert!(stats2.has_downloads());
    }

    /// Test is_noop check.
    #[test]
    fn test_pull_stats_is_noop() {
        let stats = PullStats::new();
        assert!(stats.is_noop());

        let mut stats2 = PullStats::new();
        stats2.changes_downloaded = 1;
        assert!(!stats2.is_noop());

        let mut stats3 = PullStats::new();
        stats3.changes_skipped = 1;
        assert!(!stats3.is_noop());
    }

    /// Test has_failures check.
    #[test]
    fn test_pull_stats_has_failures() {
        let mut stats = PullStats::new();
        assert!(!stats.has_failures());

        stats.changes_failed = 1;
        assert!(stats.has_failures());
    }

    /// Test has_applied check.
    #[test]
    fn test_pull_stats_has_applied() {
        let mut stats = PullStats::new();
        assert!(!stats.has_applied());

        stats.changes_applied = 3;
        assert!(stats.has_applied());
    }

    /// Test record_change_downloaded method.
    #[test]
    fn test_pull_stats_record_change_downloaded() {
        let mut stats = PullStats::new();
        stats.record_change_downloaded(1024);

        assert_eq!(stats.changes_downloaded, 1);
        assert_eq!(stats.bytes_transferred, 1024);

        stats.record_change_downloaded(2048);
        assert_eq!(stats.changes_downloaded, 2);
        assert_eq!(stats.bytes_transferred, 3072);
    }

    /// Test record_tag_downloaded method.
    #[test]
    fn test_pull_stats_record_tag_downloaded() {
        let mut stats = PullStats::new();
        stats.record_tag_downloaded(512);

        assert_eq!(stats.tags_downloaded, 1);
        assert_eq!(stats.bytes_transferred, 512);
    }

    /// Test record_skipped method.
    #[test]
    fn test_pull_stats_record_skipped() {
        let mut stats = PullStats::new();
        stats.record_skipped();
        stats.record_skipped();

        assert_eq!(stats.changes_skipped, 2);
    }

    /// Test record_failed method.
    #[test]
    fn test_pull_stats_record_failed() {
        let mut stats = PullStats::new();
        stats.record_failed();

        assert_eq!(stats.changes_failed, 1);
        assert!(stats.has_failures());
    }

    /// Test record_applied method.
    #[test]
    fn test_pull_stats_record_applied() {
        let mut stats = PullStats::new();
        stats.record_applied();
        stats.record_applied();
        stats.record_applied();

        assert_eq!(stats.changes_applied, 3);
        assert!(stats.has_applied());
    }

    /// Test merge method.
    #[test]
    fn test_pull_stats_merge() {
        let mut stats1 = PullStats::new();
        stats1.changes_downloaded = 5;
        stats1.tags_downloaded = 2;
        stats1.bytes_transferred = 1000;
        stats1.changes_skipped = 1;
        stats1.changes_failed = 0;
        stats1.changes_applied = 4;

        let mut stats2 = PullStats::new();
        stats2.changes_downloaded = 3;
        stats2.tags_downloaded = 1;
        stats2.bytes_transferred = 500;
        stats2.changes_skipped = 2;
        stats2.changes_failed = 1;
        stats2.changes_applied = 2;

        stats1.merge(&stats2);

        assert_eq!(stats1.changes_downloaded, 8);
        assert_eq!(stats1.tags_downloaded, 3);
        assert_eq!(stats1.bytes_transferred, 1500);
        assert_eq!(stats1.changes_skipped, 3);
        assert_eq!(stats1.changes_failed, 1);
        assert_eq!(stats1.changes_applied, 6);
    }

    /// Test PullStats equality.
    #[test]
    fn test_pull_stats_equality() {
        let stats1 = PullStats::new();
        let stats2 = PullStats::new();
        assert_eq!(stats1, stats2);

        let mut stats3 = PullStats::new();
        stats3.changes_downloaded = 1;
        assert_ne!(stats1, stats3);
    }

    /// Test PullStats clone.
    #[test]
    fn test_pull_stats_clone() {
        let mut original = PullStats::new();
        original.changes_downloaded = 10;
        original.bytes_transferred = 5000;

        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    // PullOutcome Tests

    /// Test creating a new outcome.
    #[test]
    fn test_pull_outcome_new() {
        let mut stats = PullStats::new();
        stats.changes_downloaded = 5;

        let outcome = PullOutcome::new(stats);

        assert_eq!(outcome.stats.changes_downloaded, 5);
        assert!(outcome.remote_state.is_none());
        assert!(outcome.local_state.is_none());
        assert!(!outcome.dry_run);
        assert!(!outcome.download_only);
        assert!(outcome.warnings.is_empty());
        assert!(outcome.local_only_changes.is_empty());
    }

    /// Test creating a dry run outcome.
    #[test]
    fn test_pull_outcome_dry_run() {
        let outcome = PullOutcome::dry_run(PullStats::new());

        assert!(outcome.dry_run);
        assert!(!outcome.download_only);
        assert!(outcome.is_success());
    }

    /// Test creating a download-only outcome.
    #[test]
    fn test_pull_outcome_download_only() {
        let outcome = PullOutcome::download_only(PullStats::new());

        assert!(outcome.download_only);
        assert!(!outcome.dry_run);
        assert!(outcome.is_success());
    }

    /// Test setting remote state.
    #[test]
    fn test_pull_outcome_with_remote_state() {
        let state = Merkle::of(b"remote");
        let outcome = PullOutcome::new(PullStats::new()).with_remote_state(state);

        assert_eq!(outcome.remote_state, Some(state));
    }

    /// Test setting local state.
    #[test]
    fn test_pull_outcome_with_local_state() {
        let state = Merkle::of(b"local");
        let outcome = PullOutcome::new(PullStats::new()).with_local_state(state);

        assert_eq!(outcome.local_state, Some(state));
    }

    /// Test adding warnings.
    #[test]
    fn test_pull_outcome_add_warning() {
        let mut outcome = PullOutcome::new(PullStats::new());
        assert!(!outcome.has_warnings());

        outcome.add_warning("Warning 1");
        outcome.add_warning("Warning 2");

        assert!(outcome.has_warnings());
        assert_eq!(outcome.warnings.len(), 2);
        assert_eq!(outcome.warnings[0], "Warning 1");
        assert_eq!(outcome.warnings[1], "Warning 2");
    }

    /// Test adding local-only changes.
    #[test]
    fn test_pull_outcome_add_local_only_change() {
        let mut outcome = PullOutcome::new(PullStats::new());
        assert!(!outcome.has_local_only_changes());
        assert_eq!(outcome.local_only_count(), 0);

        outcome.add_local_only_change("ABC123");
        outcome.add_local_only_change("DEF456");

        assert!(outcome.has_local_only_changes());
        assert_eq!(outcome.local_only_count(), 2);
        assert_eq!(outcome.local_only_changes[0], "ABC123");
    }

    /// Test is_success check.
    #[test]
    fn test_pull_outcome_is_success() {
        let outcome = PullOutcome::new(PullStats::new());
        assert!(outcome.is_success());

        let mut stats = PullStats::new();
        stats.changes_failed = 1;
        let outcome = PullOutcome::new(stats);
        assert!(!outcome.is_success());
    }

    /// Test default outcome.
    #[test]
    fn test_pull_outcome_default() {
        let outcome: PullOutcome = Default::default();

        assert!(outcome.is_success());
        assert!(!outcome.dry_run);
        assert!(!outcome.download_only);
        assert!(!outcome.has_warnings());
        assert!(!outcome.has_local_only_changes());
    }

    /// Test chaining multiple builder methods on outcome.
    #[test]
    fn test_pull_outcome_builder_chain() {
        let remote_state = Merkle::of(b"remote");
        let local_state = Merkle::of(b"local");

        let outcome = PullOutcome::new(PullStats::new())
            .with_remote_state(remote_state)
            .with_local_state(local_state);

        assert_eq!(outcome.remote_state, Some(remote_state));
        assert_eq!(outcome.local_state, Some(local_state));
    }

    /// Test that warnings don't affect success status.
    #[test]
    fn test_pull_outcome_warnings_dont_affect_success() {
        let mut outcome = PullOutcome::new(PullStats::new());
        outcome.add_warning("This is a warning");

        assert!(outcome.has_warnings());
        assert!(outcome.is_success());
    }

    /// Test that local-only changes don't affect success status.
    #[test]
    fn test_pull_outcome_local_only_changes_dont_affect_success() {
        let mut outcome = PullOutcome::new(PullStats::new());
        outcome.add_local_only_change("ABC123");

        assert!(outcome.has_local_only_changes());
        assert!(outcome.is_success());
    }
}
