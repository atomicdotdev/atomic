//! Apply context and state management for CRDT operations.
//!
//! This module provides the [`ApplyContext`] which maintains state during
//! CRDT operation application, including statistics tracking and conflict
//! management.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Apply Context Architecture                        │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  ApplyContext                                                           │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │ • options: ApplyOptions         (configuration)                 │   │
//! │  │ • stats: ApplyStats             (operation counters)            │   │
//! │  │ • conflicts: Vec<CrdtConflict>  (detected conflicts)            │   │
//! │  │ • content: Option<&[u8]>        (content blob reference)        │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  ApplyOutcome (after finish())                                          │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │ • stats: ApplyStats             (final statistics)              │   │
//! │  │ • conflicts: Vec<CrdtConflict>  (all detected conflicts)        │   │
//! │  │ • success: bool                 (whether apply succeeded)       │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Types
//!
//! - [`ApplyContext`] - Mutable state maintained during apply operations
//! - [`ApplyOutcome`] - Immutable result returned after apply completes
//! - [`ApplyStats`] - Statistics about operations processed
//!
//! # Example
//!
//! ```rust
//! use atomic_core::crdt::apply::{ApplyContext, ApplyOptions, ApplyStats};
//!
//! // Create context with options
//! let options = ApplyOptions::default();
//! let mut context = ApplyContext::new(options);
//!
//! // Track operations during apply
//! context.record_trunk_created();
//! context.record_branch_inserted();
//! context.record_leaf_inserted();
//!
//! // Finish and get outcome
//! let outcome = context.finish();
//! assert!(outcome.is_success());
//! assert_eq!(outcome.stats().trunks_created(), 1);
//! ```

use super::conflict::CrdtConflict;
use super::options::ApplyOptions;
use serde::{Deserialize, Serialize};
use std::fmt;

// ApplyStats

/// Statistics about CRDT apply operations.
///
/// Tracks counts of various operation types processed during apply.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::ApplyStats;
///
/// let mut stats = ApplyStats::new();
/// stats.add_trunk_created();
/// stats.add_branch_inserted();
/// stats.add_leaf_inserted();
///
/// assert_eq!(stats.trunks_created(), 1);
/// assert_eq!(stats.branches_inserted(), 1);
/// assert_eq!(stats.leaves_inserted(), 1);
/// assert_eq!(stats.total_operations(), 3);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyStats {
    // Trunk (file) operations
    trunks_created: u64,
    trunks_deleted: u64,
    trunks_moved: u64,
    trunks_undeleted: u64,

    // Branch (line) operations
    branches_inserted: u64,
    branches_deleted: u64,
    branches_restored: u64,

    // Leaf (token) operations
    leaves_inserted: u64,
    leaves_deleted: u64,
    leaves_replaced: u64,
    leaves_restored: u64,

    // Content tracking
    content_bytes_processed: u64,

    // Conflict tracking
    conflicts_detected: u64,

    // Skip tracking (for idempotent operations)
    operations_skipped: u64,
}

impl ApplyStats {
    /// Creates new empty statistics.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    // Trunk Operation Recording

    /// Records a trunk creation.
    #[inline]
    pub fn add_trunk_created(&mut self) {
        self.trunks_created += 1;
    }

    /// Records a trunk deletion.
    #[inline]
    pub fn add_trunk_deleted(&mut self) {
        self.trunks_deleted += 1;
    }

    /// Records a trunk move.
    #[inline]
    pub fn add_trunk_moved(&mut self) {
        self.trunks_moved += 1;
    }

    /// Records a trunk undeletion.
    #[inline]
    pub fn add_trunk_undeleted(&mut self) {
        self.trunks_undeleted += 1;
    }

    // Branch Operation Recording

    /// Records a branch insertion.
    #[inline]
    pub fn add_branch_inserted(&mut self) {
        self.branches_inserted += 1;
    }

    /// Records a branch deletion.
    #[inline]
    pub fn add_branch_deleted(&mut self) {
        self.branches_deleted += 1;
    }

    /// Records a branch restoration.
    #[inline]
    pub fn add_branch_restored(&mut self) {
        self.branches_restored += 1;
    }

    // Leaf Operation Recording

    /// Records a leaf insertion.
    #[inline]
    pub fn add_leaf_inserted(&mut self) {
        self.leaves_inserted += 1;
    }

    /// Records a leaf deletion.
    #[inline]
    pub fn add_leaf_deleted(&mut self) {
        self.leaves_deleted += 1;
    }

    /// Records a leaf replacement.
    #[inline]
    pub fn add_leaf_replaced(&mut self) {
        self.leaves_replaced += 1;
    }

    /// Records a leaf restoration.
    #[inline]
    pub fn add_leaf_restored(&mut self) {
        self.leaves_restored += 1;
    }

    // Other Recording

    /// Records content bytes processed.
    #[inline]
    pub fn add_content_bytes(&mut self, bytes: u64) {
        self.content_bytes_processed += bytes;
    }

    /// Records a conflict detection.
    #[inline]
    pub fn add_conflict(&mut self) {
        self.conflicts_detected += 1;
    }

    /// Records a skipped operation.
    #[inline]
    pub fn add_skipped(&mut self) {
        self.operations_skipped += 1;
    }

    // Accessors

    /// Returns the number of trunks created.
    #[inline]
    pub fn trunks_created(&self) -> u64 {
        self.trunks_created
    }

    /// Returns the number of trunks deleted.
    #[inline]
    pub fn trunks_deleted(&self) -> u64 {
        self.trunks_deleted
    }

    /// Returns the number of trunks moved.
    #[inline]
    pub fn trunks_moved(&self) -> u64 {
        self.trunks_moved
    }

    /// Returns the number of trunks undeleted.
    #[inline]
    pub fn trunks_undeleted(&self) -> u64 {
        self.trunks_undeleted
    }

    /// Returns the number of branches inserted.
    #[inline]
    pub fn branches_inserted(&self) -> u64 {
        self.branches_inserted
    }

    /// Returns the number of branches deleted.
    #[inline]
    pub fn branches_deleted(&self) -> u64 {
        self.branches_deleted
    }

    /// Returns the number of branches restored.
    #[inline]
    pub fn branches_restored(&self) -> u64 {
        self.branches_restored
    }

    /// Returns the number of leaves inserted.
    #[inline]
    pub fn leaves_inserted(&self) -> u64 {
        self.leaves_inserted
    }

    /// Returns the number of leaves deleted.
    #[inline]
    pub fn leaves_deleted(&self) -> u64 {
        self.leaves_deleted
    }

    /// Returns the number of leaves replaced.
    #[inline]
    pub fn leaves_replaced(&self) -> u64 {
        self.leaves_replaced
    }

    /// Returns the number of leaves restored.
    #[inline]
    pub fn leaves_restored(&self) -> u64 {
        self.leaves_restored
    }

    /// Returns the number of content bytes processed.
    #[inline]
    pub fn content_bytes_processed(&self) -> u64 {
        self.content_bytes_processed
    }

    /// Returns the number of conflicts detected.
    #[inline]
    pub fn conflicts_detected(&self) -> u64 {
        self.conflicts_detected
    }

    /// Returns the number of operations skipped.
    #[inline]
    pub fn operations_skipped(&self) -> u64 {
        self.operations_skipped
    }

    // Aggregate Accessors

    /// Returns the total number of trunk operations.
    #[inline]
    pub fn total_trunk_ops(&self) -> u64 {
        self.trunks_created + self.trunks_deleted + self.trunks_moved + self.trunks_undeleted
    }

    /// Returns the total number of branch operations.
    #[inline]
    pub fn total_branch_ops(&self) -> u64 {
        self.branches_inserted + self.branches_deleted + self.branches_restored
    }

    /// Returns the total number of leaf operations.
    #[inline]
    pub fn total_leaf_ops(&self) -> u64 {
        self.leaves_inserted + self.leaves_deleted + self.leaves_replaced + self.leaves_restored
    }

    /// Returns the total number of all operations.
    #[inline]
    pub fn total_operations(&self) -> u64 {
        self.total_trunk_ops() + self.total_branch_ops() + self.total_leaf_ops()
    }

    /// Returns `true` if any operations were processed.
    #[inline]
    pub fn has_operations(&self) -> bool {
        self.total_operations() > 0
    }

    /// Returns `true` if any conflicts were detected.
    #[inline]
    pub fn has_conflicts(&self) -> bool {
        self.conflicts_detected > 0
    }

    // Merging

    /// Merges another stats instance into this one.
    ///
    /// This is useful when combining stats from parallel operations.
    pub fn merge(&mut self, other: &ApplyStats) {
        self.trunks_created += other.trunks_created;
        self.trunks_deleted += other.trunks_deleted;
        self.trunks_moved += other.trunks_moved;
        self.trunks_undeleted += other.trunks_undeleted;
        self.branches_inserted += other.branches_inserted;
        self.branches_deleted += other.branches_deleted;
        self.branches_restored += other.branches_restored;
        self.leaves_inserted += other.leaves_inserted;
        self.leaves_deleted += other.leaves_deleted;
        self.leaves_replaced += other.leaves_replaced;
        self.leaves_restored += other.leaves_restored;
        self.content_bytes_processed += other.content_bytes_processed;
        self.conflicts_detected += other.conflicts_detected;
        self.operations_skipped += other.operations_skipped;
    }
}

impl fmt::Display for ApplyStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "trunks: +{} -{} ~{} | branches: +{} -{} | leaves: +{} -{} ~{}",
            self.trunks_created,
            self.trunks_deleted,
            self.trunks_moved,
            self.branches_inserted,
            self.branches_deleted,
            self.leaves_inserted,
            self.leaves_deleted,
            self.leaves_replaced
        )
    }
}

// ApplyContext

/// Mutable context maintained during CRDT apply operations.
///
/// The context tracks:
/// - Configuration options
/// - Operation statistics
/// - Detected conflicts
/// - Content blob reference
///
/// # Lifecycle
///
/// 1. Create with [`ApplyContext::new`] or `ApplyContext::with_content`
/// 2. Use during apply operations to record stats and conflicts
/// 3. Call [`ApplyContext::finish`] to get the final [`ApplyOutcome`]
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::{ApplyContext, ApplyOptions};
///
/// let options = ApplyOptions::default();
/// let mut context = ApplyContext::new(options);
///
/// // During apply operations...
/// context.record_trunk_created();
/// context.record_branch_inserted();
///
/// // Finish and get outcome
/// let outcome = context.finish();
/// assert_eq!(outcome.stats().total_operations(), 2);
/// ```
#[derive(Debug)]
pub struct ApplyContext {
    /// Configuration options.
    options: ApplyOptions,

    /// Operation statistics.
    stats: ApplyStats,

    /// Detected conflicts.
    conflicts: Vec<CrdtConflict>,

    /// Whether the apply has failed.
    failed: bool,

    /// Failure reason, if any.
    failure_reason: Option<String>,
}

impl ApplyContext {
    /// Creates a new context with the given options.
    #[inline]
    pub fn new(options: ApplyOptions) -> Self {
        Self {
            options,
            stats: ApplyStats::new(),
            conflicts: Vec::new(),
            failed: false,
            failure_reason: None,
        }
    }

    /// Creates a new context with default options.
    #[inline]
    pub fn with_defaults() -> Self {
        Self::new(ApplyOptions::default())
    }

    // Accessors

    /// Returns a reference to the options.
    #[inline]
    pub fn options(&self) -> &ApplyOptions {
        &self.options
    }

    /// Returns a reference to the current statistics.
    #[inline]
    pub fn stats(&self) -> &ApplyStats {
        &self.stats
    }

    /// Returns a mutable reference to the statistics.
    #[inline]
    pub fn stats_mut(&mut self) -> &mut ApplyStats {
        &mut self.stats
    }

    /// Returns the detected conflicts.
    #[inline]
    pub fn conflicts(&self) -> &[CrdtConflict] {
        &self.conflicts
    }

    /// Returns `true` if the apply has failed.
    #[inline]
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Returns the failure reason, if any.
    #[inline]
    pub fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }

    // Conflict Management

    /// Adds a conflict to the context.
    ///
    /// If `fail_on_conflict` is enabled in options, marks the apply as failed.
    pub fn add_conflict(&mut self, conflict: CrdtConflict) {
        self.stats.add_conflict();
        self.conflicts.push(conflict);

        if self.options.fail_on_conflict() {
            self.failed = true;
            self.failure_reason = Some("Conflict detected".to_string());
        }
    }

    /// Returns `true` if any conflicts have been detected.
    #[inline]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Returns the number of conflicts detected.
    #[inline]
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    // Failure Management

    /// Marks the apply as failed with a reason.
    pub fn mark_failed(&mut self, reason: impl Into<String>) {
        self.failed = true;
        self.failure_reason = Some(reason.into());
    }

    // Stat Recording Convenience Methods

    /// Records a trunk creation.
    #[inline]
    pub fn record_trunk_created(&mut self) {
        self.stats.add_trunk_created();
    }

    /// Records a trunk deletion.
    #[inline]
    pub fn record_trunk_deleted(&mut self) {
        self.stats.add_trunk_deleted();
    }

    /// Records a trunk move.
    #[inline]
    pub fn record_trunk_moved(&mut self) {
        self.stats.add_trunk_moved();
    }

    /// Records a trunk undeletion.
    #[inline]
    pub fn record_trunk_undeleted(&mut self) {
        self.stats.add_trunk_undeleted();
    }

    /// Records a branch insertion.
    #[inline]
    pub fn record_branch_inserted(&mut self) {
        self.stats.add_branch_inserted();
    }

    /// Records a branch deletion.
    #[inline]
    pub fn record_branch_deleted(&mut self) {
        self.stats.add_branch_deleted();
    }

    /// Records a branch restoration.
    #[inline]
    pub fn record_branch_restored(&mut self) {
        self.stats.add_branch_restored();
    }

    /// Records a leaf insertion.
    #[inline]
    pub fn record_leaf_inserted(&mut self) {
        self.stats.add_leaf_inserted();
    }

    /// Records a leaf deletion.
    #[inline]
    pub fn record_leaf_deleted(&mut self) {
        self.stats.add_leaf_deleted();
    }

    /// Records a leaf replacement.
    #[inline]
    pub fn record_leaf_replaced(&mut self) {
        self.stats.add_leaf_replaced();
    }

    /// Records a leaf restoration.
    #[inline]
    pub fn record_leaf_restored(&mut self) {
        self.stats.add_leaf_restored();
    }

    /// Records content bytes processed.
    #[inline]
    pub fn record_content_bytes(&mut self, bytes: u64) {
        self.stats.add_content_bytes(bytes);
    }

    /// Records a skipped operation.
    #[inline]
    pub fn record_skipped(&mut self) {
        self.stats.add_skipped();
    }

    // Validation Helpers

    /// Returns `true` if the operation limit has been exceeded.
    #[inline]
    pub fn exceeds_operation_limit(&self) -> bool {
        self.options
            .exceeds_limit(self.stats.total_operations() as usize)
    }

    /// Checks if we should continue applying operations.
    ///
    /// Returns `false` if:
    /// - The apply has failed
    /// - The operation limit has been exceeded
    #[inline]
    pub fn should_continue(&self) -> bool {
        !self.failed && !self.exceeds_operation_limit()
    }

    // Finalization

    /// Consumes the context and returns the final outcome.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::crdt::apply::{ApplyContext, ApplyOptions};
    ///
    /// let mut context = ApplyContext::new(ApplyOptions::default());
    /// context.record_trunk_created();
    ///
    /// let outcome = context.finish();
    /// assert!(outcome.is_success());
    /// ```
    pub fn finish(self) -> ApplyOutcome {
        ApplyOutcome {
            stats: self.stats,
            conflicts: self.conflicts,
            success: !self.failed,
            failure_reason: self.failure_reason,
        }
    }
}

// ApplyOutcome

/// The result of applying CRDT operations.
///
/// Contains final statistics, any detected conflicts, and success status.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::{ApplyContext, ApplyOptions};
///
/// let mut context = ApplyContext::new(ApplyOptions::default());
/// context.record_trunk_created();
/// context.record_branch_inserted();
///
/// let outcome = context.finish();
///
/// if outcome.is_success() {
///     println!("Applied {} operations", outcome.stats().total_operations());
/// }
///
/// if outcome.has_conflicts() {
///     for conflict in outcome.conflicts() {
///         println!("Conflict: {:?}", conflict);
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    /// Final operation statistics.
    stats: ApplyStats,

    /// All detected conflicts.
    conflicts: Vec<CrdtConflict>,

    /// Whether the apply succeeded.
    success: bool,

    /// Failure reason, if any.
    failure_reason: Option<String>,
}

impl ApplyOutcome {
    /// Returns `true` if the apply succeeded.
    #[inline]
    pub fn is_success(&self) -> bool {
        self.success
    }

    /// Returns `true` if the apply failed.
    #[inline]
    pub fn is_failure(&self) -> bool {
        !self.success
    }

    /// Returns a reference to the statistics.
    #[inline]
    pub fn stats(&self) -> &ApplyStats {
        &self.stats
    }

    /// Returns the detected conflicts.
    #[inline]
    pub fn conflicts(&self) -> &[CrdtConflict] {
        &self.conflicts
    }

    /// Returns `true` if any conflicts were detected.
    #[inline]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Returns the number of conflicts.
    #[inline]
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// Returns the failure reason, if any.
    #[inline]
    pub fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }

    /// Consumes the outcome and returns the statistics.
    #[inline]
    pub fn into_stats(self) -> ApplyStats {
        self.stats
    }

    /// Consumes the outcome and returns the conflicts.
    #[inline]
    pub fn into_conflicts(self) -> Vec<CrdtConflict> {
        self.conflicts
    }

    /// Consumes the outcome and returns both stats and conflicts.
    #[inline]
    pub fn into_parts(self) -> (ApplyStats, Vec<CrdtConflict>) {
        (self.stats, self.conflicts)
    }
}

impl fmt::Display for ApplyOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.success {
            write!(f, "Success: {}", self.stats)?;
        } else {
            write!(f, "Failed")?;
            if let Some(reason) = &self.failure_reason {
                write!(f, ": {}", reason)?;
            }
        }

        if !self.conflicts.is_empty() {
            write!(f, " ({} conflicts)", self.conflicts.len())?;
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // ApplyStats Tests

    #[test]
    fn test_stats_new() {
        let stats = ApplyStats::new();
        assert_eq!(stats.total_operations(), 0);
        assert!(!stats.has_operations());
        assert!(!stats.has_conflicts());
    }

    #[test]
    fn test_stats_default() {
        let stats = ApplyStats::default();
        assert_eq!(stats, ApplyStats::new());
    }

    #[test]
    fn test_stats_trunk_operations() {
        let mut stats = ApplyStats::new();

        stats.add_trunk_created();
        assert_eq!(stats.trunks_created(), 1);

        stats.add_trunk_deleted();
        assert_eq!(stats.trunks_deleted(), 1);

        stats.add_trunk_moved();
        assert_eq!(stats.trunks_moved(), 1);

        stats.add_trunk_undeleted();
        assert_eq!(stats.trunks_undeleted(), 1);

        assert_eq!(stats.total_trunk_ops(), 4);
    }

    #[test]
    fn test_stats_branch_operations() {
        let mut stats = ApplyStats::new();

        stats.add_branch_inserted();
        assert_eq!(stats.branches_inserted(), 1);

        stats.add_branch_deleted();
        assert_eq!(stats.branches_deleted(), 1);

        stats.add_branch_restored();
        assert_eq!(stats.branches_restored(), 1);

        assert_eq!(stats.total_branch_ops(), 3);
    }

    #[test]
    fn test_stats_leaf_operations() {
        let mut stats = ApplyStats::new();

        stats.add_leaf_inserted();
        assert_eq!(stats.leaves_inserted(), 1);

        stats.add_leaf_deleted();
        assert_eq!(stats.leaves_deleted(), 1);

        stats.add_leaf_replaced();
        assert_eq!(stats.leaves_replaced(), 1);

        stats.add_leaf_restored();
        assert_eq!(stats.leaves_restored(), 1);

        assert_eq!(stats.total_leaf_ops(), 4);
    }

    #[test]
    fn test_stats_total_operations() {
        let mut stats = ApplyStats::new();

        stats.add_trunk_created();
        stats.add_branch_inserted();
        stats.add_leaf_inserted();

        assert_eq!(stats.total_operations(), 3);
        assert!(stats.has_operations());
    }

    #[test]
    fn test_stats_content_bytes() {
        let mut stats = ApplyStats::new();
        stats.add_content_bytes(100);
        stats.add_content_bytes(50);
        assert_eq!(stats.content_bytes_processed(), 150);
    }

    #[test]
    fn test_stats_conflicts() {
        let mut stats = ApplyStats::new();
        assert!(!stats.has_conflicts());

        stats.add_conflict();
        assert!(stats.has_conflicts());
        assert_eq!(stats.conflicts_detected(), 1);
    }

    #[test]
    fn test_stats_skipped() {
        let mut stats = ApplyStats::new();
        stats.add_skipped();
        stats.add_skipped();
        assert_eq!(stats.operations_skipped(), 2);
    }

    #[test]
    fn test_stats_merge() {
        let mut stats1 = ApplyStats::new();
        stats1.add_trunk_created();
        stats1.add_branch_inserted();

        let mut stats2 = ApplyStats::new();
        stats2.add_trunk_created();
        stats2.add_leaf_inserted();
        stats2.add_conflict();

        stats1.merge(&stats2);

        assert_eq!(stats1.trunks_created(), 2);
        assert_eq!(stats1.branches_inserted(), 1);
        assert_eq!(stats1.leaves_inserted(), 1);
        assert_eq!(stats1.conflicts_detected(), 1);
    }

    #[test]
    fn test_stats_display() {
        let mut stats = ApplyStats::new();
        stats.add_trunk_created();
        stats.add_trunk_deleted();
        stats.add_branch_inserted();
        stats.add_leaf_inserted();
        stats.add_leaf_replaced();

        let display = stats.to_string();
        assert!(display.contains("trunks"));
        assert!(display.contains("branches"));
        assert!(display.contains("leaves"));
    }

    #[test]
    fn test_stats_serde_roundtrip() {
        let mut stats = ApplyStats::new();
        stats.add_trunk_created();
        stats.add_branch_inserted();
        stats.add_leaf_inserted();
        stats.add_content_bytes(42);

        let json = serde_json::to_string(&stats).unwrap();
        let restored: ApplyStats = serde_json::from_str(&json).unwrap();

        assert_eq!(stats, restored);
    }

    // ApplyContext Tests

    #[test]
    fn test_context_new() {
        let options = ApplyOptions::default();
        let context = ApplyContext::new(options.clone());

        assert_eq!(context.options(), &options);
        assert_eq!(context.stats().total_operations(), 0);
        assert!(context.conflicts().is_empty());
        assert!(!context.has_failed());
    }

    #[test]
    fn test_context_with_defaults() {
        let context = ApplyContext::with_defaults();
        assert_eq!(context.options(), &ApplyOptions::default());
    }

    #[test]
    fn test_context_record_operations() {
        let mut context = ApplyContext::with_defaults();

        context.record_trunk_created();
        context.record_trunk_deleted();
        context.record_trunk_moved();
        context.record_trunk_undeleted();
        context.record_branch_inserted();
        context.record_branch_deleted();
        context.record_branch_restored();
        context.record_leaf_inserted();
        context.record_leaf_deleted();
        context.record_leaf_replaced();
        context.record_leaf_restored();
        context.record_content_bytes(100);
        context.record_skipped();

        assert_eq!(context.stats().trunks_created(), 1);
        assert_eq!(context.stats().trunks_deleted(), 1);
        assert_eq!(context.stats().trunks_moved(), 1);
        assert_eq!(context.stats().trunks_undeleted(), 1);
        assert_eq!(context.stats().branches_inserted(), 1);
        assert_eq!(context.stats().branches_deleted(), 1);
        assert_eq!(context.stats().branches_restored(), 1);
        assert_eq!(context.stats().leaves_inserted(), 1);
        assert_eq!(context.stats().leaves_deleted(), 1);
        assert_eq!(context.stats().leaves_replaced(), 1);
        assert_eq!(context.stats().leaves_restored(), 1);
        assert_eq!(context.stats().content_bytes_processed(), 100);
        assert_eq!(context.stats().operations_skipped(), 1);
    }

    #[test]
    fn test_context_add_conflict() {
        use super::super::conflict::{ConflictKind, CrdtConflict};
        use crate::crdt::BranchId;
        use crate::types::NodeId;

        let options = ApplyOptions::builder().fail_on_conflict(false).build();
        let mut context = ApplyContext::new(options);

        let conflict =
            CrdtConflict::new(ConflictKind::ConcurrentInsert, "test conflict".to_string());
        context.add_conflict(conflict);

        assert!(context.has_conflicts());
        assert_eq!(context.conflict_count(), 1);
        assert!(!context.has_failed()); // fail_on_conflict is false
    }

    #[test]
    fn test_context_add_conflict_with_fail() {
        use super::super::conflict::{ConflictKind, CrdtConflict};

        let options = ApplyOptions::builder().fail_on_conflict(true).build();
        let mut context = ApplyContext::new(options);

        let conflict =
            CrdtConflict::new(ConflictKind::ConcurrentInsert, "test conflict".to_string());
        context.add_conflict(conflict);

        assert!(context.has_conflicts());
        assert!(context.has_failed());
        assert!(context.failure_reason().is_some());
    }

    #[test]
    fn test_context_mark_failed() {
        let mut context = ApplyContext::with_defaults();

        assert!(!context.has_failed());
        context.mark_failed("test failure");
        assert!(context.has_failed());
        assert_eq!(context.failure_reason(), Some("test failure"));
    }

    #[test]
    fn test_context_should_continue() {
        let mut context = ApplyContext::with_defaults();
        assert!(context.should_continue());

        context.mark_failed("test");
        assert!(!context.should_continue());
    }

    #[test]
    fn test_context_exceeds_operation_limit() {
        let options = ApplyOptions::builder().max_operations(Some(2)).build();
        let mut context = ApplyContext::new(options);

        assert!(!context.exceeds_operation_limit());
        context.record_trunk_created();
        assert!(!context.exceeds_operation_limit());
        context.record_branch_inserted();
        assert!(context.exceeds_operation_limit());
    }

    #[test]
    fn test_context_finish_success() {
        let mut context = ApplyContext::with_defaults();
        context.record_trunk_created();
        context.record_branch_inserted();

        let outcome = context.finish();

        assert!(outcome.is_success());
        assert!(!outcome.is_failure());
        assert_eq!(outcome.stats().total_operations(), 2);
        assert!(!outcome.has_conflicts());
    }

    #[test]
    fn test_context_finish_failure() {
        let mut context = ApplyContext::with_defaults();
        context.mark_failed("test failure reason");

        let outcome = context.finish();

        assert!(outcome.is_failure());
        assert!(!outcome.is_success());
        assert_eq!(outcome.failure_reason(), Some("test failure reason"));
    }

    #[test]
    fn test_context_stats_mut() {
        let mut context = ApplyContext::with_defaults();
        context.stats_mut().add_trunk_created();
        assert_eq!(context.stats().trunks_created(), 1);
    }

    // ApplyOutcome Tests

    #[test]
    fn test_outcome_success() {
        let context = ApplyContext::with_defaults();
        let outcome = context.finish();

        assert!(outcome.is_success());
        assert!(!outcome.has_conflicts());
        assert!(outcome.failure_reason().is_none());
    }

    #[test]
    fn test_outcome_with_conflicts() {
        use super::super::conflict::{ConflictKind, CrdtConflict};

        let options = ApplyOptions::builder().fail_on_conflict(false).build();
        let mut context = ApplyContext::new(options);

        let conflict = CrdtConflict::new(ConflictKind::ConcurrentInsert, "test".to_string());
        context.add_conflict(conflict);

        let outcome = context.finish();

        assert!(outcome.is_success()); // success because fail_on_conflict is false
        assert!(outcome.has_conflicts());
        assert_eq!(outcome.conflict_count(), 1);
    }

    #[test]
    fn test_outcome_into_stats() {
        let mut context = ApplyContext::with_defaults();
        context.record_trunk_created();

        let outcome = context.finish();
        let stats = outcome.into_stats();

        assert_eq!(stats.trunks_created(), 1);
    }

    #[test]
    fn test_outcome_into_conflicts() {
        use super::super::conflict::{ConflictKind, CrdtConflict};

        let options = ApplyOptions::builder().fail_on_conflict(false).build();
        let mut context = ApplyContext::new(options);

        let conflict = CrdtConflict::new(ConflictKind::ConcurrentInsert, "test".to_string());
        context.add_conflict(conflict);

        let outcome = context.finish();
        let conflicts = outcome.into_conflicts();

        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn test_outcome_into_parts() {
        use super::super::conflict::{ConflictKind, CrdtConflict};

        let options = ApplyOptions::builder().fail_on_conflict(false).build();
        let mut context = ApplyContext::new(options);
        context.record_trunk_created();

        let conflict = CrdtConflict::new(ConflictKind::ConcurrentInsert, "test".to_string());
        context.add_conflict(conflict);

        let outcome = context.finish();
        let (stats, conflicts) = outcome.into_parts();

        assert_eq!(stats.trunks_created(), 1);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn test_outcome_display_success() {
        let mut context = ApplyContext::with_defaults();
        context.record_trunk_created();

        let outcome = context.finish();
        let display = outcome.to_string();

        assert!(display.contains("Success"));
    }

    #[test]
    fn test_outcome_display_failure() {
        let mut context = ApplyContext::with_defaults();
        context.mark_failed("test reason");

        let outcome = context.finish();
        let display = outcome.to_string();

        assert!(display.contains("Failed"));
        assert!(display.contains("test reason"));
    }

    #[test]
    fn test_outcome_display_with_conflicts() {
        use super::super::conflict::{ConflictKind, CrdtConflict};

        let options = ApplyOptions::builder().fail_on_conflict(false).build();
        let mut context = ApplyContext::new(options);

        let conflict = CrdtConflict::new(ConflictKind::ConcurrentInsert, "test".to_string());
        context.add_conflict(conflict);

        let outcome = context.finish();
        let display = outcome.to_string();

        assert!(display.contains("conflict"));
    }

    #[test]
    fn test_outcome_clone() {
        let mut context = ApplyContext::with_defaults();
        context.record_trunk_created();

        let outcome = context.finish();
        let cloned = outcome.clone();

        assert_eq!(outcome.is_success(), cloned.is_success());
        assert_eq!(
            outcome.stats().trunks_created(),
            cloned.stats().trunks_created()
        );
    }
}
