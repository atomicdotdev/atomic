#![allow(dead_code)]
//! Types for the clone command.
//!
//! This module defines the data structures used throughout the clone operation,
//! including representations of clone progress, statistics tracking, and outcomes.
//!
//! # Overview
//!
//! The clone command uses three main types:
//!
//! - [`CloneProgress`]: Tracks the progress of an ongoing clone operation
//! - [`CloneStats`]: Tracks statistics about the clone operation (downloads, bytes, etc.)
//! - [`CloneOutcome`]: The final result of a clone operation
//!
//! # Design Philosophy
//!
//! These types are designed to be:
//!
//! 1. **Immutable by default**: Use builder methods to construct instances
//! 2. **Testable**: All types implement common traits like `Debug`, `Clone`, `PartialEq`
//! 3. **Self-documenting**: Rich documentation with examples
//! 4. **Type-safe**: Leverage Rust's type system to prevent invalid states

use std::path::PathBuf;

use atomic_core::types::Merkle;

// =============================================================================
// CloneProgress
// =============================================================================

/// Tracks the progress of an ongoing clone operation.
///
/// This struct is used to report progress to the user during the clone,
/// showing how many changes have been downloaded and applied.
///
/// # Example
///
/// ```rust
/// use atomic::commands::clone::types::CloneProgress;
///
/// let mut progress = CloneProgress::new(100);
/// assert_eq!(progress.total_changes, 100);
/// assert_eq!(progress.downloaded, 0);
///
/// progress.record_downloaded();
/// assert_eq!(progress.downloaded, 1);
/// assert!(!progress.is_complete());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneProgress {
    /// Total number of changes to download.
    pub total_changes: usize,

    /// Number of changes downloaded so far.
    pub downloaded: usize,

    /// Number of changes applied so far.
    pub applied: usize,

    /// Number of tags downloaded so far.
    pub tags_downloaded: usize,

    /// Current phase of the clone operation.
    pub phase: ClonePhase,
}

/// The current phase of a clone operation.
///
/// Used to track which stage of the clone process is currently executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClonePhase {
    /// Initial phase - not yet started.
    #[default]
    NotStarted,

    /// Connecting to the remote server.
    Connecting,

    /// Querying remote state and changelist.
    QueryingRemote,

    /// Downloading changes from the remote.
    Downloading,

    /// Applying downloaded changes to the local stack.
    Applying,

    /// Downloading tag files.
    DownloadingTags,

    /// Configuring the remote in repository settings.
    ConfiguringRemote,

    /// Clone operation completed successfully.
    Complete,

    /// Clone operation failed.
    Failed,
}

impl CloneProgress {
    /// Create a new progress tracker for the given number of changes.
    ///
    /// # Arguments
    ///
    /// * `total_changes` - The total number of changes to download
    ///
    /// # Returns
    ///
    /// A new `CloneProgress` instance with all counters at zero.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneProgress;
    ///
    /// let progress = CloneProgress::new(50);
    /// assert_eq!(progress.total_changes, 50);
    /// assert_eq!(progress.downloaded, 0);
    /// ```
    pub fn new(total_changes: usize) -> Self {
        Self {
            total_changes,
            downloaded: 0,
            applied: 0,
            tags_downloaded: 0,
            phase: ClonePhase::NotStarted,
        }
    }

    /// Set the current phase.
    ///
    /// # Arguments
    ///
    /// * `phase` - The new phase to set
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::{CloneProgress, ClonePhase};
    ///
    /// let progress = CloneProgress::new(10).with_phase(ClonePhase::Downloading);
    /// assert_eq!(progress.phase, ClonePhase::Downloading);
    /// ```
    pub fn with_phase(mut self, phase: ClonePhase) -> Self {
        self.phase = phase;
        self
    }

    /// Record a successfully downloaded change.
    ///
    /// Increments the download counter.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneProgress;
    ///
    /// let mut progress = CloneProgress::new(10);
    /// progress.record_downloaded();
    /// assert_eq!(progress.downloaded, 1);
    /// ```
    pub fn record_downloaded(&mut self) {
        self.downloaded += 1;
    }

    /// Record a successfully applied change.
    ///
    /// Increments the applied counter.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneProgress;
    ///
    /// let mut progress = CloneProgress::new(10);
    /// progress.record_applied();
    /// assert_eq!(progress.applied, 1);
    /// ```
    pub fn record_applied(&mut self) {
        self.applied += 1;
    }

    /// Record a successfully downloaded tag.
    ///
    /// Increments the tags counter.
    pub fn record_tag_downloaded(&mut self) {
        self.tags_downloaded += 1;
    }

    /// Check if all changes have been downloaded.
    ///
    /// # Returns
    ///
    /// `true` if downloaded equals total_changes.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneProgress;
    ///
    /// let mut progress = CloneProgress::new(2);
    /// assert!(!progress.downloads_complete());
    ///
    /// progress.record_downloaded();
    /// progress.record_downloaded();
    /// assert!(progress.downloads_complete());
    /// ```
    pub fn downloads_complete(&self) -> bool {
        self.downloaded >= self.total_changes
    }

    /// Check if all changes have been applied.
    ///
    /// # Returns
    ///
    /// `true` if applied equals total_changes.
    pub fn applies_complete(&self) -> bool {
        self.applied >= self.total_changes
    }

    /// Check if the clone operation is complete.
    ///
    /// # Returns
    ///
    /// `true` if the phase is `Complete`.
    pub fn is_complete(&self) -> bool {
        self.phase == ClonePhase::Complete
    }

    /// Get the download progress as a percentage (0-100).
    ///
    /// # Returns
    ///
    /// The percentage of changes downloaded, or 0 if total_changes is 0.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneProgress;
    ///
    /// let mut progress = CloneProgress::new(4);
    /// assert_eq!(progress.download_percent(), 0);
    ///
    /// progress.record_downloaded();
    /// assert_eq!(progress.download_percent(), 25);
    ///
    /// progress.record_downloaded();
    /// progress.record_downloaded();
    /// progress.record_downloaded();
    /// assert_eq!(progress.download_percent(), 100);
    /// ```
    pub fn download_percent(&self) -> u8 {
        if self.total_changes == 0 {
            return 0;
        }
        ((self.downloaded * 100) / self.total_changes) as u8
    }
}

impl Default for CloneProgress {
    fn default() -> Self {
        Self::new(0)
    }
}

// =============================================================================
// CloneStats
// =============================================================================

/// Statistics about a clone operation.
///
/// Tracks metrics about what was cloned, useful for reporting to the user
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
/// use atomic::commands::clone::types::CloneStats;
///
/// let mut stats = CloneStats::new();
/// assert_eq!(stats.changes_downloaded, 0);
/// assert_eq!(stats.total_downloaded(), 0);
///
/// stats.changes_downloaded = 5;
/// stats.tags_downloaded = 2;
/// assert_eq!(stats.total_downloaded(), 7);
/// assert!(stats.has_downloads());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloneStats {
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
    /// This should normally be 0 for a fresh clone, but could be non-zero
    /// if resuming a failed clone.
    pub changes_skipped: usize,

    /// Number of changes that failed to download.
    ///
    /// Counts download failures due to network errors, authentication
    /// issues, or other problems.
    pub changes_failed: usize,

    /// Number of changes successfully applied to the local stack.
    ///
    /// This may differ from `changes_downloaded` if `--download-only` is used,
    /// or if some changes fail to apply.
    pub changes_applied: usize,
}

impl CloneStats {
    /// Create new empty statistics.
    ///
    /// All counters are initialized to zero.
    ///
    /// # Returns
    ///
    /// A new `CloneStats` instance with all fields set to zero.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let stats = CloneStats::new();
    /// assert_eq!(stats.changes_downloaded, 0);
    /// assert_eq!(stats.bytes_transferred, 0);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Get total number of items downloaded (changes + tags).
    ///
    /// # Returns
    ///
    /// The sum of downloaded changes and tags.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
    /// stats.changes_downloaded = 10;
    /// stats.tags_downloaded = 3;
    ///
    /// assert_eq!(stats.total_downloaded(), 13);
    /// ```
    pub fn total_downloaded(&self) -> usize {
        self.changes_downloaded + self.tags_downloaded
    }

    /// Check if any items were downloaded.
    ///
    /// # Returns
    ///
    /// `true` if at least one change or tag was downloaded.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
    /// assert!(!stats.has_downloads());
    ///
    /// stats.changes_downloaded = 1;
    /// assert!(stats.has_downloads());
    /// ```
    pub fn has_downloads(&self) -> bool {
        self.total_downloaded() > 0
    }

    /// Check if nothing happened (no downloads, no skips, no failures).
    ///
    /// This indicates that the remote was empty.
    ///
    /// # Returns
    ///
    /// `true` if all counters are zero.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let stats = CloneStats::new();
    /// assert!(stats.is_noop());
    ///
    /// let mut stats2 = CloneStats::new();
    /// stats2.changes_downloaded = 1;
    /// assert!(!stats2.is_noop());
    /// ```
    pub fn is_noop(&self) -> bool {
        self.changes_downloaded == 0
            && self.tags_downloaded == 0
            && self.changes_skipped == 0
            && self.changes_failed == 0
            && self.changes_applied == 0
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
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
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
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
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
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
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

    /// Record a successfully downloaded tag.
    ///
    /// Increments the tag counter and adds the bytes to the transfer total.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The size of the downloaded tag file in bytes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
    /// stats.record_tag_downloaded(512);
    ///
    /// assert_eq!(stats.tags_downloaded, 1);
    /// assert_eq!(stats.bytes_transferred, 512);
    /// ```
    pub fn record_tag_downloaded(&mut self, bytes: u64) {
        self.tags_downloaded += 1;
        self.bytes_transferred += bytes;
    }

    /// Record a skipped change (already exists locally).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
    /// stats.record_skipped();
    ///
    /// assert_eq!(stats.changes_skipped, 1);
    /// ```
    pub fn record_skipped(&mut self) {
        self.changes_skipped += 1;
    }

    /// Record a failed download.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
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
    /// use atomic::commands::clone::types::CloneStats;
    ///
    /// let mut stats = CloneStats::new();
    /// stats.record_applied();
    /// stats.record_applied();
    ///
    /// assert_eq!(stats.changes_applied, 2);
    /// assert!(stats.has_applied());
    /// ```
    pub fn record_applied(&mut self) {
        self.changes_applied += 1;
    }
}

// =============================================================================
// CloneOutcome
// =============================================================================

/// The outcome of a clone operation.
///
/// Contains statistics about what was downloaded and applied, along with
/// metadata about the operation itself (was it a dry run? any warnings?).
///
/// # Example
///
/// ```rust
/// use atomic::commands::clone::types::{CloneOutcome, CloneStats};
/// use atomic_core::types::Merkle;
/// use std::path::PathBuf;
///
/// let stats = CloneStats::new();
/// let outcome = CloneOutcome::new(stats, PathBuf::from("/tmp/repo"))
///     .with_remote_state(Merkle::ZERO);
///
/// assert!(outcome.is_success());
/// assert!(!outcome.download_only);
/// ```
#[derive(Debug, Clone, Default)]
pub struct CloneOutcome {
    /// Statistics about the clone operation.
    pub stats: CloneStats,

    /// The path where the repository was cloned.
    pub target_path: PathBuf,

    /// The remote's Merkle state after the clone.
    ///
    /// This is useful for displaying the final state or for verification.
    pub remote_state: Option<Merkle>,

    /// The local Merkle state after the clone.
    ///
    /// Should match the remote state after a successful clone.
    pub local_state: Option<Merkle>,

    /// The stack that was cloned.
    pub stack: String,

    /// The remote URL that was cloned from.
    pub remote_url: String,

    /// Whether only downloads were performed (no apply).
    pub download_only: bool,

    /// Warning messages generated during the operation.
    ///
    /// Warnings don't prevent the operation from completing, but inform
    /// the user about potential issues.
    pub warnings: Vec<String>,
}

impl CloneOutcome {
    /// Create a new clone outcome with the given statistics and target path.
    ///
    /// # Arguments
    ///
    /// * `stats` - The statistics from the clone operation
    /// * `target_path` - The path where the repository was cloned
    ///
    /// # Returns
    ///
    /// A new `CloneOutcome` instance.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::{CloneOutcome, CloneStats};
    /// use std::path::PathBuf;
    ///
    /// let mut stats = CloneStats::new();
    /// stats.changes_downloaded = 5;
    ///
    /// let outcome = CloneOutcome::new(stats, PathBuf::from("/tmp/repo"));
    /// assert_eq!(outcome.stats.changes_downloaded, 5);
    /// ```
    pub fn new(stats: CloneStats, target_path: PathBuf) -> Self {
        Self {
            stats,
            target_path,
            remote_state: None,
            local_state: None,
            stack: String::new(),
            remote_url: String::new(),
            download_only: false,
            warnings: Vec::new(),
        }
    }

    /// Create a download-only outcome with the given statistics.
    ///
    /// A download-only outcome indicates that changes were downloaded
    /// but not applied to any local stack.
    ///
    /// # Arguments
    ///
    /// * `stats` - The download statistics
    /// * `target_path` - The path where the repository was cloned
    ///
    /// # Returns
    ///
    /// A new `CloneOutcome` with `download_only = true`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::{CloneOutcome, CloneStats};
    /// use std::path::PathBuf;
    ///
    /// let outcome = CloneOutcome::download_only(CloneStats::new(), PathBuf::from("/tmp/repo"));
    /// assert!(outcome.download_only);
    /// ```
    pub fn download_only(stats: CloneStats, target_path: PathBuf) -> Self {
        Self {
            stats,
            target_path,
            download_only: true,
            ..Default::default()
        }
    }

    /// Set the remote state.
    ///
    /// # Arguments
    ///
    /// * `state` - The remote's current Merkle state
    ///
    /// # Returns
    ///
    /// Self with the remote state set, enabling method chaining.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::{CloneOutcome, CloneStats};
    /// use atomic_core::types::Merkle;
    /// use std::path::PathBuf;
    ///
    /// let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"))
    ///     .with_remote_state(Merkle::ZERO);
    ///
    /// assert!(outcome.remote_state.is_some());
    /// ```
    pub fn with_remote_state(mut self, state: Merkle) -> Self {
        self.remote_state = Some(state);
        self
    }

    /// Set the local state.
    ///
    /// # Arguments
    ///
    /// * `state` - The local Merkle state after clone
    ///
    /// # Returns
    ///
    /// Self with the local state set, enabling method chaining.
    pub fn with_local_state(mut self, state: Merkle) -> Self {
        self.local_state = Some(state);
        self
    }

    /// Set the stack name.
    ///
    /// # Arguments
    ///
    /// * `stack` - The name of the cloned stack
    ///
    /// # Returns
    ///
    /// Self with the stack set, enabling method chaining.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = stack.into();
        self
    }

    /// Set the remote URL.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL that was cloned from
    ///
    /// # Returns
    ///
    /// Self with the remote URL set, enabling method chaining.
    pub fn with_remote_url(mut self, url: impl Into<String>) -> Self {
        self.remote_url = url.into();
        self
    }

    /// Add a warning message.
    ///
    /// Warnings inform the user about potential issues but don't
    /// prevent the operation from completing.
    ///
    /// # Arguments
    ///
    /// * `warning` - The warning message
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::{CloneOutcome, CloneStats};
    /// use std::path::PathBuf;
    ///
    /// let mut outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"));
    /// outcome.add_warning("Some files could not be written");
    ///
    /// assert!(outcome.has_warnings());
    /// assert_eq!(outcome.warnings.len(), 1);
    /// ```
    pub fn add_warning(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    /// Check if the operation was successful.
    ///
    /// An operation is considered successful if there were no failures.
    ///
    /// # Returns
    ///
    /// `true` if the operation completed without failures.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::types::{CloneOutcome, CloneStats};
    /// use std::path::PathBuf;
    ///
    /// let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"));
    /// assert!(outcome.is_success());
    ///
    /// let mut stats = CloneStats::new();
    /// stats.changes_failed = 1;
    /// let outcome = CloneOutcome::new(stats, PathBuf::from("/tmp/repo"));
    /// assert!(!outcome.is_success());
    /// ```
    pub fn is_success(&self) -> bool {
        !self.stats.has_failures()
    }

    /// Check if there are any warnings.
    ///
    /// # Returns
    ///
    /// `true` if at least one warning was recorded.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Check if states match (remote and local are synchronized).
    ///
    /// # Returns
    ///
    /// `true` if both states are present and equal, `false` otherwise.
    pub fn states_match(&self) -> bool {
        match (&self.remote_state, &self.local_state) {
            (Some(remote), Some(local)) => remote == local,
            _ => false,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // ClonePhase Tests
    // =========================================================================

    /// Test ClonePhase default value.
    #[test]
    fn test_clone_phase_default() {
        let phase: ClonePhase = Default::default();
        assert_eq!(phase, ClonePhase::NotStarted);
    }

    /// Test ClonePhase equality.
    #[test]
    fn test_clone_phase_equality() {
        assert_eq!(ClonePhase::Downloading, ClonePhase::Downloading);
        assert_ne!(ClonePhase::Downloading, ClonePhase::Applying);
    }

    /// Test ClonePhase clone.
    #[test]
    fn test_clone_phase_clone() {
        let phase = ClonePhase::Downloading;
        let cloned = phase;
        assert_eq!(phase, cloned);
    }

    // =========================================================================
    // CloneProgress Tests
    // =========================================================================

    /// Test creating a CloneProgress with total changes.
    #[test]
    fn test_clone_progress_new() {
        let progress = CloneProgress::new(100);

        assert_eq!(progress.total_changes, 100);
        assert_eq!(progress.downloaded, 0);
        assert_eq!(progress.applied, 0);
        assert_eq!(progress.tags_downloaded, 0);
        assert_eq!(progress.phase, ClonePhase::NotStarted);
    }

    /// Test CloneProgress default.
    #[test]
    fn test_clone_progress_default() {
        let progress: CloneProgress = Default::default();
        assert_eq!(progress.total_changes, 0);
        assert_eq!(progress.downloaded, 0);
    }

    /// Test with_phase builder method.
    #[test]
    fn test_clone_progress_with_phase() {
        let progress = CloneProgress::new(10).with_phase(ClonePhase::Downloading);
        assert_eq!(progress.phase, ClonePhase::Downloading);
    }

    /// Test record_downloaded method.
    #[test]
    fn test_clone_progress_record_downloaded() {
        let mut progress = CloneProgress::new(10);
        progress.record_downloaded();
        progress.record_downloaded();

        assert_eq!(progress.downloaded, 2);
    }

    /// Test record_applied method.
    #[test]
    fn test_clone_progress_record_applied() {
        let mut progress = CloneProgress::new(10);
        progress.record_applied();

        assert_eq!(progress.applied, 1);
    }

    /// Test record_tag_downloaded method.
    #[test]
    fn test_clone_progress_record_tag_downloaded() {
        let mut progress = CloneProgress::new(10);
        progress.record_tag_downloaded();
        progress.record_tag_downloaded();

        assert_eq!(progress.tags_downloaded, 2);
    }

    /// Test downloads_complete check.
    #[test]
    fn test_clone_progress_downloads_complete() {
        let mut progress = CloneProgress::new(2);
        assert!(!progress.downloads_complete());

        progress.record_downloaded();
        assert!(!progress.downloads_complete());

        progress.record_downloaded();
        assert!(progress.downloads_complete());
    }

    /// Test applies_complete check.
    #[test]
    fn test_clone_progress_applies_complete() {
        let mut progress = CloneProgress::new(2);
        assert!(!progress.applies_complete());

        progress.record_applied();
        progress.record_applied();
        assert!(progress.applies_complete());
    }

    /// Test is_complete check.
    #[test]
    fn test_clone_progress_is_complete() {
        let mut progress = CloneProgress::new(1);
        assert!(!progress.is_complete());

        progress.phase = ClonePhase::Complete;
        assert!(progress.is_complete());
    }

    /// Test download_percent calculation.
    #[test]
    fn test_clone_progress_download_percent() {
        let mut progress = CloneProgress::new(4);
        assert_eq!(progress.download_percent(), 0);

        progress.record_downloaded();
        assert_eq!(progress.download_percent(), 25);

        progress.record_downloaded();
        assert_eq!(progress.download_percent(), 50);

        progress.record_downloaded();
        progress.record_downloaded();
        assert_eq!(progress.download_percent(), 100);
    }

    /// Test download_percent with zero total.
    #[test]
    fn test_clone_progress_download_percent_zero_total() {
        let progress = CloneProgress::new(0);
        assert_eq!(progress.download_percent(), 0);
    }

    /// Test CloneProgress equality.
    #[test]
    fn test_clone_progress_equality() {
        let p1 = CloneProgress::new(10);
        let p2 = CloneProgress::new(10);
        assert_eq!(p1, p2);

        let mut p3 = CloneProgress::new(10);
        p3.record_downloaded();
        assert_ne!(p1, p3);
    }

    /// Test CloneProgress clone.
    #[test]
    fn test_clone_progress_clone() {
        let original = CloneProgress::new(10).with_phase(ClonePhase::Downloading);
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    // =========================================================================
    // CloneStats Tests
    // =========================================================================

    /// Test creating empty statistics.
    #[test]
    fn test_clone_stats_new() {
        let stats = CloneStats::new();

        assert_eq!(stats.changes_downloaded, 0);
        assert_eq!(stats.tags_downloaded, 0);
        assert_eq!(stats.bytes_transferred, 0);
        assert_eq!(stats.changes_skipped, 0);
        assert_eq!(stats.changes_failed, 0);
        assert_eq!(stats.changes_applied, 0);
    }

    /// Test Default implementation for CloneStats.
    #[test]
    fn test_clone_stats_default() {
        let stats: CloneStats = Default::default();
        assert!(stats.is_noop());
    }

    /// Test total_downloaded calculation.
    #[test]
    fn test_clone_stats_total_downloaded() {
        let mut stats = CloneStats::new();
        assert_eq!(stats.total_downloaded(), 0);

        stats.changes_downloaded = 10;
        assert_eq!(stats.total_downloaded(), 10);

        stats.tags_downloaded = 5;
        assert_eq!(stats.total_downloaded(), 15);
    }

    /// Test has_downloads check.
    #[test]
    fn test_clone_stats_has_downloads() {
        let mut stats = CloneStats::new();
        assert!(!stats.has_downloads());

        stats.changes_downloaded = 1;
        assert!(stats.has_downloads());

        let mut stats2 = CloneStats::new();
        stats2.tags_downloaded = 1;
        assert!(stats2.has_downloads());
    }

    /// Test is_noop check.
    #[test]
    fn test_clone_stats_is_noop() {
        let stats = CloneStats::new();
        assert!(stats.is_noop());

        let mut stats2 = CloneStats::new();
        stats2.changes_downloaded = 1;
        assert!(!stats2.is_noop());

        let mut stats3 = CloneStats::new();
        stats3.changes_skipped = 1;
        assert!(!stats3.is_noop());
    }

    /// Test has_failures check.
    #[test]
    fn test_clone_stats_has_failures() {
        let mut stats = CloneStats::new();
        assert!(!stats.has_failures());

        stats.changes_failed = 1;
        assert!(stats.has_failures());
    }

    /// Test has_applied check.
    #[test]
    fn test_clone_stats_has_applied() {
        let mut stats = CloneStats::new();
        assert!(!stats.has_applied());

        stats.changes_applied = 3;
        assert!(stats.has_applied());
    }

    /// Test record_change_downloaded method.
    #[test]
    fn test_clone_stats_record_change_downloaded() {
        let mut stats = CloneStats::new();
        stats.record_change_downloaded(1024);

        assert_eq!(stats.changes_downloaded, 1);
        assert_eq!(stats.bytes_transferred, 1024);

        stats.record_change_downloaded(2048);
        assert_eq!(stats.changes_downloaded, 2);
        assert_eq!(stats.bytes_transferred, 3072);
    }

    /// Test record_tag_downloaded method.
    #[test]
    fn test_clone_stats_record_tag_downloaded() {
        let mut stats = CloneStats::new();
        stats.record_tag_downloaded(512);

        assert_eq!(stats.tags_downloaded, 1);
        assert_eq!(stats.bytes_transferred, 512);
    }

    /// Test record_skipped method.
    #[test]
    fn test_clone_stats_record_skipped() {
        let mut stats = CloneStats::new();
        stats.record_skipped();
        stats.record_skipped();

        assert_eq!(stats.changes_skipped, 2);
    }

    /// Test record_failed method.
    #[test]
    fn test_clone_stats_record_failed() {
        let mut stats = CloneStats::new();
        stats.record_failed();

        assert_eq!(stats.changes_failed, 1);
        assert!(stats.has_failures());
    }

    /// Test record_applied method.
    #[test]
    fn test_clone_stats_record_applied() {
        let mut stats = CloneStats::new();
        stats.record_applied();
        stats.record_applied();
        stats.record_applied();

        assert_eq!(stats.changes_applied, 3);
        assert!(stats.has_applied());
    }

    /// Test CloneStats equality.
    #[test]
    fn test_clone_stats_equality() {
        let stats1 = CloneStats::new();
        let stats2 = CloneStats::new();
        assert_eq!(stats1, stats2);

        let mut stats3 = CloneStats::new();
        stats3.changes_downloaded = 1;
        assert_ne!(stats1, stats3);
    }

    /// Test CloneStats clone.
    #[test]
    fn test_clone_stats_clone() {
        let mut original = CloneStats::new();
        original.changes_downloaded = 10;
        original.bytes_transferred = 5000;

        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    // =========================================================================
    // CloneOutcome Tests
    // =========================================================================

    /// Test creating a new outcome.
    #[test]
    fn test_clone_outcome_new() {
        let mut stats = CloneStats::new();
        stats.changes_downloaded = 5;

        let outcome = CloneOutcome::new(stats, PathBuf::from("/tmp/repo"));

        assert_eq!(outcome.stats.changes_downloaded, 5);
        assert_eq!(outcome.target_path, PathBuf::from("/tmp/repo"));
        assert!(outcome.remote_state.is_none());
        assert!(outcome.local_state.is_none());
        assert!(outcome.stack.is_empty());
        assert!(outcome.remote_url.is_empty());
        assert!(!outcome.download_only);
        assert!(outcome.warnings.is_empty());
    }

    /// Test creating a download-only outcome.
    #[test]
    fn test_clone_outcome_download_only() {
        let outcome = CloneOutcome::download_only(CloneStats::new(), PathBuf::from("/tmp/repo"));

        assert!(outcome.download_only);
        assert!(outcome.is_success());
    }

    /// Test setting remote state.
    #[test]
    fn test_clone_outcome_with_remote_state() {
        let state = Merkle::of(b"remote");
        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"))
            .with_remote_state(state);

        assert_eq!(outcome.remote_state, Some(state));
    }

    /// Test setting local state.
    #[test]
    fn test_clone_outcome_with_local_state() {
        let state = Merkle::of(b"local");
        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"))
            .with_local_state(state);

        assert_eq!(outcome.local_state, Some(state));
    }

    /// Test setting stack.
    #[test]
    fn test_clone_outcome_with_stack() {
        let outcome =
            CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo")).with_stack("main");

        assert_eq!(outcome.stack, "main");
    }

    /// Test setting remote URL.
    #[test]
    fn test_clone_outcome_with_remote_url() {
        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"))
            .with_remote_url("https://example.com/repo");

        assert_eq!(outcome.remote_url, "https://example.com/repo");
    }

    /// Test adding warnings.
    #[test]
    fn test_clone_outcome_add_warning() {
        let mut outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"));
        assert!(!outcome.has_warnings());

        outcome.add_warning("Warning 1");
        outcome.add_warning("Warning 2");

        assert!(outcome.has_warnings());
        assert_eq!(outcome.warnings.len(), 2);
        assert_eq!(outcome.warnings[0], "Warning 1");
        assert_eq!(outcome.warnings[1], "Warning 2");
    }

    /// Test is_success check.
    #[test]
    fn test_clone_outcome_is_success() {
        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"));
        assert!(outcome.is_success());

        let mut stats = CloneStats::new();
        stats.changes_failed = 1;
        let outcome = CloneOutcome::new(stats, PathBuf::from("/tmp/repo"));
        assert!(!outcome.is_success());
    }

    /// Test default outcome.
    #[test]
    fn test_clone_outcome_default() {
        let outcome: CloneOutcome = Default::default();

        assert!(outcome.is_success());
        assert!(!outcome.download_only);
        assert!(!outcome.has_warnings());
        assert!(outcome.target_path.as_os_str().is_empty());
    }

    /// Test chaining multiple builder methods on outcome.
    #[test]
    fn test_clone_outcome_builder_chain() {
        let remote_state = Merkle::of(b"remote");
        let local_state = Merkle::of(b"local");

        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"))
            .with_remote_state(remote_state)
            .with_local_state(local_state)
            .with_stack("main")
            .with_remote_url("https://example.com/repo");

        assert_eq!(outcome.remote_state, Some(remote_state));
        assert_eq!(outcome.local_state, Some(local_state));
        assert_eq!(outcome.stack, "main");
        assert_eq!(outcome.remote_url, "https://example.com/repo");
    }

    /// Test that warnings don't affect success status.
    #[test]
    fn test_clone_outcome_warnings_dont_affect_success() {
        let mut outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"));
        outcome.add_warning("This is a warning");

        assert!(outcome.has_warnings());
        assert!(outcome.is_success());
    }

    /// Test states_match when both states are equal.
    #[test]
    fn test_clone_outcome_states_match_equal() {
        let state = Merkle::of(b"same");
        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"))
            .with_remote_state(state)
            .with_local_state(state);

        assert!(outcome.states_match());
    }

    /// Test states_match when states differ.
    #[test]
    fn test_clone_outcome_states_match_different() {
        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"))
            .with_remote_state(Merkle::of(b"remote"))
            .with_local_state(Merkle::of(b"local"));

        assert!(!outcome.states_match());
    }

    /// Test states_match when states are missing.
    #[test]
    fn test_clone_outcome_states_match_missing() {
        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"));
        assert!(!outcome.states_match());

        let outcome = CloneOutcome::new(CloneStats::new(), PathBuf::from("/tmp/repo"))
            .with_remote_state(Merkle::ZERO);
        assert!(!outcome.states_match());
    }
}
