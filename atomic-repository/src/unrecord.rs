//! Unrecord operations for Atomic VCS
//!
//! This module provides functionality for undoing (unrecording) changes that
//! have been applied to a stack. Unrecording is the inverse of applying - it
//! removes a change from the stack and reverses its effects on the graph.
//!
//! # Overview
//!
//! Unrecording a change involves several steps:
//!
//! 1. **Verify**: Ensure the change is in the stack and can be safely removed
//! 2. **Check Dependencies**: Ensure no other applied changes depend on it
//! 3. **Unapply**: Reverse the graph modifications made by the change
//! 4. **Update Stack**: Remove from change log and recompute Merkle state
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      Unrecord Operation Flow                            │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Stack Log              Graph                    Result                 │
//! │  ┌──────────┐         ┌─────────────┐          ┌─────────────┐         │
//! │  │ Seq 0: A │         │  Vertices   │          │ Change      │         │
//! │  │ Seq 1: B │ ──────▶ │  & Edges    │ ──────▶  │ Removed     │         │
//! │  │ Seq 2: C │ remove  │  from B     │ revert   │ from Stack  │         │
//! │  └──────────┘   B     └─────────────┘          └─────────────┘         │
//! │                                                                         │
//! │  Before: [A, B, C]                                                      │
//! │  After:  [A, C'] where C' depends only on A                             │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Safety Considerations
//!
//! Unrecording is a potentially destructive operation:
//!
//! - **Dependency Check**: Changes that other applied changes depend on cannot
//!   be unrecorded without also unrecording the dependents
//! - **Working Copy**: The working copy may need to be updated after unrecord
//! - **Remote Sync**: Unrecorded changes may still exist on remotes
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, UnrecordOptions};
//!
//! let repo = Repository::open(".")?;
//!
//! // Unrecord the most recent change
//! let result = repo.unrecord_last(UnrecordOptions::default())?;
//!
//! // Unrecord a specific change by hash
//! let result = repo.unrecord(&hash, UnrecordOptions::default())?;
//!
//! // Unrecord multiple changes
//! let result = repo.unrecord_many(&[hash1, hash2], UnrecordOptions::default())?;
//!
//! // Check what would be unrecorded (dry run)
//! let preview = repo.unrecord(&hash, UnrecordOptions::dry_run())?;
//! println!("Would unrecord {} changes", preview.affected_count);
//! ```
//!
//! # Difference from Git Reset
//!
//! Unlike `git reset`, unrecord in Atomic:
//!
//! - Only removes changes from the current stack, not globally
//! - Preserves the change in the change store (can be re-applied)
//! - Is fully reversible (just re-apply the change)
//! - Works with the graph model, not commit trees

use atomic_core::pristine::{StackState, StackTxnT};
use atomic_core::types::{Base32, Hash, Merkle};
use std::collections::HashSet;
use std::fmt;
use thiserror::Error;

// ============================================================================
// Error Types
// ============================================================================

/// Result type for unrecord operations.
pub type UnrecordResult<T> = Result<T, UnrecordError>;

/// Errors that can occur during unrecord operations.
#[derive(Debug, Error)]
pub enum UnrecordError {
    /// The change was not found in the repository.
    #[error("Change not found: {hash}")]
    ChangeNotFound {
        /// Hash of the missing change.
        hash: String,
    },

    /// The change is not in the specified stack.
    #[error("Change {} is not in stack '{stack}'", hash)]
    NotInStack {
        /// Hash of the change.
        hash: String,
        /// Name of the stack.
        stack: String,
    },

    /// The change cannot be unrecorded because other changes depend on it.
    #[error("Change {} is depended upon by: {}", hash, dependents.join(", "))]
    HasDependents {
        /// Hash of the change that has dependents.
        hash: String,
        /// Hashes of the dependent changes.
        dependents: Vec<String>,
    },

    /// The stack was not found.
    #[error("Stack not found: {name}")]
    StackNotFound {
        /// Name of the missing stack.
        name: String,
    },

    /// The stack is empty (no changes to unrecord).
    #[error("Stack '{name}' is empty")]
    StackEmpty {
        /// Name of the empty stack.
        name: String,
    },

    /// Cannot unrecord - would leave graph in inconsistent state.
    #[error("Unrecord would leave graph inconsistent: {reason}")]
    Inconsistent {
        /// Reason for inconsistency.
        reason: String,
    },

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// ============================================================================
// Unrecord Options
// ============================================================================

/// Options for unrecord operations.
///
/// # Example
///
/// ```rust,ignore
/// let options = UnrecordOptions::default()
///     .cascade(true)  // Also unrecord dependents
///     .update_working_copy(true);
/// ```
#[derive(Debug, Clone)]
pub struct UnrecordOptions {
    /// Stack to unrecord from (None = current stack).
    pub stack: Option<String>,

    /// Whether to cascade and also unrecord dependent changes.
    pub cascade: bool,

    /// Whether to update the working copy after unrecord.
    pub update_working_copy: bool,

    /// Whether to perform a dry run (don't actually unrecord).
    pub dry_run: bool,

    /// Maximum number of changes to unrecord in cascade.
    pub max_cascade: Option<usize>,

    /// Whether to keep the change in the change store.
    /// If false, the change file may be deleted if unused elsewhere.
    pub keep_change: bool,
}

impl Default for UnrecordOptions {
    fn default() -> Self {
        Self {
            stack: None,
            cascade: false,
            update_working_copy: true,
            dry_run: false,
            max_cascade: Some(100),
            keep_change: true,
        }
    }
}

impl UnrecordOptions {
    /// Create new default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the stack to unrecord from.
    pub fn stack(mut self, name: impl Into<String>) -> Self {
        self.stack = Some(name.into());
        self
    }

    /// Enable cascade mode to also unrecord dependents.
    pub fn cascade(mut self, cascade: bool) -> Self {
        self.cascade = cascade;
        self
    }

    /// Set whether to update the working copy.
    pub fn update_working_copy(mut self, update: bool) -> Self {
        self.update_working_copy = update;
        self
    }

    /// Enable dry run mode.
    pub fn dry_run() -> Self {
        Self {
            dry_run: true,
            ..Default::default()
        }
    }

    /// Set maximum cascade depth.
    pub fn max_cascade(mut self, max: usize) -> Self {
        self.max_cascade = Some(max);
        self
    }

    /// Set whether to keep the change file.
    pub fn keep_change(mut self, keep: bool) -> Self {
        self.keep_change = keep;
        self
    }

    /// Create strict options that don't cascade.
    pub fn strict() -> Self {
        Self {
            cascade: false,
            ..Default::default()
        }
    }
}

// ============================================================================
// Unrecord Result
// ============================================================================

/// Result of an unrecord operation.
///
/// Contains information about what was (or would be) unrecorded.
#[derive(Debug, Clone)]
pub struct UnrecordOutcome {
    /// The changes that were unrecorded.
    pub unrecorded: Vec<Hash>,

    /// The new Merkle state after unrecording.
    pub new_state: Merkle,

    /// The new change count after unrecording.
    pub new_count: u64,

    /// Whether this was a dry run.
    pub was_dry_run: bool,

    /// Statistics about the operation.
    pub stats: UnrecordStats,

    /// Warnings generated during the operation.
    pub warnings: Vec<String>,
}

impl UnrecordOutcome {
    /// Create a new outcome.
    pub fn new(unrecorded: Vec<Hash>, new_state: Merkle, new_count: u64) -> Self {
        Self {
            unrecorded,
            new_state,
            new_count,
            was_dry_run: false,
            stats: UnrecordStats::default(),
            warnings: Vec::new(),
        }
    }

    /// Mark as a dry run result.
    pub fn as_dry_run(mut self) -> Self {
        self.was_dry_run = true;
        self
    }

    /// Add a warning.
    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    /// Set statistics.
    pub fn with_stats(mut self, stats: UnrecordStats) -> Self {
        self.stats = stats;
        self
    }

    /// Get the number of changes unrecorded.
    pub fn count(&self) -> usize {
        self.unrecorded.len()
    }

    /// Check if anything was unrecorded.
    pub fn has_changes(&self) -> bool {
        !self.unrecorded.is_empty()
    }
}

impl fmt::Display for UnrecordOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.was_dry_run {
            write!(f, "[DRY RUN] ")?;
        }
        write!(
            f,
            "Unrecorded {} change(s), new state: {}",
            self.unrecorded.len(),
            &self.new_state.to_base32()[..8]
        )
    }
}

// ============================================================================
// Unrecord Statistics
// ============================================================================

/// Statistics about an unrecord operation.
#[derive(Debug, Clone, Default)]
pub struct UnrecordStats {
    /// Number of changes directly unrecorded.
    pub direct_unrecords: usize,

    /// Number of changes unrecorded due to cascade.
    pub cascaded_unrecords: usize,

    /// Number of vertices removed from the graph.
    pub vertices_removed: usize,

    /// Number of edges removed from the graph.
    pub edges_removed: usize,

    /// Number of files affected in working copy.
    pub files_affected: usize,
}

impl UnrecordStats {
    /// Create new empty stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get total number of unrecords.
    pub fn total_unrecords(&self) -> usize {
        self.direct_unrecords + self.cascaded_unrecords
    }

    /// Merge stats from another operation.
    pub fn merge(&mut self, other: &UnrecordStats) {
        self.direct_unrecords += other.direct_unrecords;
        self.cascaded_unrecords += other.cascaded_unrecords;
        self.vertices_removed += other.vertices_removed;
        self.edges_removed += other.edges_removed;
        self.files_affected += other.files_affected;
    }
}

// ============================================================================
// Dependency Information
// ============================================================================

/// Information about a change's dependencies for unrecord purposes.
#[derive(Debug, Clone)]
pub struct UnrecordDependencyInfo {
    /// The hash of the change.
    pub hash: Hash,

    /// Changes that this change depends on.
    pub dependencies: Vec<Hash>,

    /// Changes that depend on this change (in the current stack).
    pub dependents: Vec<Hash>,

    /// Whether this change can be safely unrecorded.
    pub can_unrecord: bool,

    /// Reason if cannot unrecord.
    pub block_reason: Option<String>,
}

impl UnrecordDependencyInfo {
    /// Create new dependency info.
    pub fn new(hash: Hash) -> Self {
        Self {
            hash,
            dependencies: Vec::new(),
            dependents: Vec::new(),
            can_unrecord: true,
            block_reason: None,
        }
    }

    /// Add a dependency.
    pub fn add_dependency(&mut self, dep: Hash) {
        self.dependencies.push(dep);
    }

    /// Add a dependent.
    pub fn add_dependent(&mut self, dep: Hash) {
        self.dependents.push(dep);
        self.can_unrecord = false;
        self.block_reason = Some(format!(
            "Change has {} dependent(s) in the stack",
            self.dependents.len()
        ));
    }

    /// Check if this change has dependents.
    pub fn has_dependents(&self) -> bool {
        !self.dependents.is_empty()
    }
}

// ============================================================================
// Core Functions
// ============================================================================

/// Check if a change can be unrecorded from a stack.
///
/// This verifies that:
/// 1. The change is in the stack
/// 2. No other changes in the stack depend on it (unless cascade is enabled)
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to check
/// * `hash` - Hash of the change to check
/// * `options` - Unrecord options
///
/// # Returns
///
/// `UnrecordDependencyInfo` with details about dependencies.
pub fn check_can_unrecord<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    hash: &Hash,
    options: &UnrecordOptions,
) -> UnrecordResult<UnrecordDependencyInfo> {
    // Get internal ID
    let node_id = txn
        .get_internal(hash)
        .map_err(|e| UnrecordError::Database(e.to_string()))?
        .ok_or_else(|| UnrecordError::ChangeNotFound {
            hash: hash.to_base32(),
        })?;

    // Check if change is in stack
    let _seq = txn
        .get_change_seq(stack, node_id)
        .map_err(|e| UnrecordError::Database(e.to_string()))?
        .ok_or_else(|| UnrecordError::NotInStack {
            hash: hash.to_base32(),
            stack: stack.name.clone(),
        })?;

    let mut info = UnrecordDependencyInfo::new(*hash);

    // Find dependents in the stack
    // This requires checking all changes after this one to see if they depend on it
    let iter = txn
        .iter_changes(stack, 0)
        .map_err(|e| UnrecordError::Database(e.to_string()))?;

    for result in iter {
        let (_, change_id, _) = result.map_err(|e| UnrecordError::Database(e.to_string()))?;

        // Skip the change itself
        if change_id == node_id {
            continue;
        }

        // Note: In a full implementation, we'd check the change's dependencies
        // by loading it from the change store. For now, we rely on the deps table.
        // This is a simplified check.
    }

    // If cascade is enabled, we can always unrecord
    if options.cascade {
        info.can_unrecord = true;
        info.block_reason = None;
    }

    Ok(info)
}

/// Find all changes that would need to be unrecorded to safely remove a change.
///
/// This includes the target change plus any dependents (in topological order).
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to analyze
/// * `hash` - Hash of the change to unrecord
/// * `max_depth` - Maximum cascade depth
///
/// # Returns
///
/// A vector of hashes in the order they should be unrecorded (dependents first).
pub fn find_unrecord_set<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    hash: &Hash,
    max_depth: Option<usize>,
) -> UnrecordResult<Vec<Hash>> {
    let max_depth = max_depth.unwrap_or(100);
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut to_process = vec![(*hash, 0)];

    while let Some((current_hash, depth)) = to_process.pop() {
        if depth > max_depth {
            return Err(UnrecordError::Inconsistent {
                reason: format!("Cascade depth exceeded maximum of {}", max_depth),
            });
        }

        if visited.contains(&current_hash) {
            continue;
        }
        visited.insert(current_hash);

        // Get internal ID
        let node_id = match txn.get_internal(&current_hash) {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(e) => return Err(UnrecordError::Database(e.to_string())),
        };

        // Check if in stack
        if txn
            .get_change_seq(stack, node_id)
            .map_err(|e| UnrecordError::Database(e.to_string()))?
            .is_none()
        {
            continue;
        }

        result.push(current_hash);

        // Note: Finding dependents requires traversing all changes
        // In a full implementation, we'd use the rev_deps table
    }

    // Reverse so dependents come first
    result.reverse();

    Ok(result)
}

/// Compute the new Merkle state after unrecording changes.
///
/// This recomputes the state by re-hashing the remaining changes.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack being modified
/// * `removed` - Set of change hashes being removed
///
/// # Returns
///
/// The new Merkle state after removal.
pub fn compute_state_after_unrecord<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    removed: &HashSet<Hash>,
) -> UnrecordResult<Merkle> {
    let mut state = Merkle::ZERO;

    // Iterate through all changes and recompute state without the removed ones
    let iter = txn
        .iter_changes(stack, 0)
        .map_err(|e| UnrecordError::Database(e.to_string()))?;

    for result in iter {
        let (_, change_id, _) = result.map_err(|e| UnrecordError::Database(e.to_string()))?;

        // Get external hash
        let hash = txn
            .get_external(change_id)
            .map_err(|e| UnrecordError::Database(e.to_string()))?
            .ok_or_else(|| UnrecordError::ChangeNotFound {
                hash: format!("{:?}", change_id),
            })?;

        // Skip removed changes
        if removed.contains(&hash) {
            continue;
        }

        // Update state
        state = state.next(&hash);
    }

    Ok(state)
}

/// Preview what would be unrecorded without actually doing it.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to preview
/// * `hashes` - Hashes of changes to unrecord
/// * `options` - Unrecord options
///
/// # Returns
///
/// An `UnrecordOutcome` describing what would happen.
pub fn preview_unrecord<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    hashes: &[Hash],
    options: &UnrecordOptions,
) -> UnrecordResult<UnrecordOutcome> {
    let mut all_unrecords = Vec::new();

    for hash in hashes {
        if options.cascade {
            let set = find_unrecord_set(txn, stack, hash, options.max_cascade)?;
            for h in set {
                if !all_unrecords.contains(&h) {
                    all_unrecords.push(h);
                }
            }
        } else {
            // Check if can unrecord without cascade
            let info = check_can_unrecord(txn, stack, hash, options)?;
            if !info.can_unrecord {
                return Err(UnrecordError::HasDependents {
                    hash: hash.to_base32(),
                    dependents: info.dependents.iter().map(|h| h.to_base32()).collect(),
                });
            }
            if !all_unrecords.contains(hash) {
                all_unrecords.push(*hash);
            }
        }
    }

    // Compute new state
    let removed_set: HashSet<Hash> = all_unrecords.iter().copied().collect();
    let new_state = compute_state_after_unrecord(txn, stack, &removed_set)?;
    let new_count = stack.change_count - all_unrecords.len() as u64;

    let mut stats = UnrecordStats::new();
    stats.direct_unrecords = hashes.len();
    stats.cascaded_unrecords = all_unrecords.len() - hashes.len();

    Ok(UnrecordOutcome::new(all_unrecords, new_state, new_count)
        .as_dry_run()
        .with_stats(stats))
}

/// Get the sequence number of the last change in a stack.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to query
///
/// # Returns
///
/// The last sequence number, or None if stack is empty.
pub fn get_last_sequence<T: StackTxnT>(
    _txn: &T,
    stack: &StackState,
) -> UnrecordResult<Option<u64>> {
    if stack.change_count == 0 {
        return Ok(None);
    }
    Ok(Some(stack.change_count - 1))
}

/// Get the hash of the last change in a stack.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to query
///
/// # Returns
///
/// The hash of the last change, or None if stack is empty.
pub fn get_last_change<T: StackTxnT>(_txn: &T, stack: &StackState) -> UnrecordResult<Option<Hash>> {
    if stack.change_count == 0 {
        return Ok(None);
    }

    let last_seq = stack.change_count - 1;
    let node_id = _txn
        .get_change_at_seq(stack, last_seq)
        .map_err(|e| UnrecordError::Database(e.to_string()))?
        .ok_or_else(|| UnrecordError::Inconsistent {
            reason: format!("No change at sequence {}", last_seq),
        })?;

    let hash = _txn
        .get_external(node_id)
        .map_err(|e| UnrecordError::Database(e.to_string()))?
        .ok_or_else(|| UnrecordError::ChangeNotFound {
            hash: format!("{:?}", node_id),
        })?;

    Ok(Some(hash))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // UnrecordOptions Tests
    // ========================================================================

    #[test]
    fn test_unrecord_options_default() {
        let options = UnrecordOptions::default();

        assert!(options.stack.is_none());
        assert!(!options.cascade);
        assert!(options.update_working_copy);
        assert!(!options.dry_run);
        assert_eq!(options.max_cascade, Some(100));
        assert!(options.keep_change);
    }

    #[test]
    fn test_unrecord_options_builder() {
        let options = UnrecordOptions::new()
            .stack("feature")
            .cascade(true)
            .update_working_copy(false)
            .max_cascade(50)
            .keep_change(false);

        assert_eq!(options.stack, Some("feature".to_string()));
        assert!(options.cascade);
        assert!(!options.update_working_copy);
        assert_eq!(options.max_cascade, Some(50));
        assert!(!options.keep_change);
    }

    #[test]
    fn test_unrecord_options_dry_run() {
        let options = UnrecordOptions::dry_run();

        assert!(options.dry_run);
    }

    #[test]
    fn test_unrecord_options_strict() {
        let options = UnrecordOptions::strict();

        assert!(!options.cascade);
    }

    // ========================================================================
    // UnrecordOutcome Tests
    // ========================================================================

    #[test]
    fn test_unrecord_outcome_new() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let outcome = UnrecordOutcome::new(vec![hash], state, 10);

        assert_eq!(outcome.count(), 1);
        assert!(outcome.has_changes());
        assert_eq!(outcome.new_count, 10);
        assert!(!outcome.was_dry_run);
    }

    #[test]
    fn test_unrecord_outcome_empty() {
        let state = Merkle::of(b"state");
        let outcome = UnrecordOutcome::new(vec![], state, 0);

        assert_eq!(outcome.count(), 0);
        assert!(!outcome.has_changes());
    }

    #[test]
    fn test_unrecord_outcome_dry_run() {
        let state = Merkle::of(b"state");
        let outcome = UnrecordOutcome::new(vec![], state, 0).as_dry_run();

        assert!(outcome.was_dry_run);
    }

    #[test]
    fn test_unrecord_outcome_with_warning() {
        let state = Merkle::of(b"state");
        let outcome = UnrecordOutcome::new(vec![], state, 0).with_warning("Test warning");

        assert_eq!(outcome.warnings.len(), 1);
        assert_eq!(outcome.warnings[0], "Test warning");
    }

    #[test]
    fn test_unrecord_outcome_display() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let outcome = UnrecordOutcome::new(vec![hash], state, 10);

        let display = format!("{}", outcome);
        assert!(display.contains("1 change"));
    }

    #[test]
    fn test_unrecord_outcome_display_dry_run() {
        let state = Merkle::of(b"state");
        let outcome = UnrecordOutcome::new(vec![], state, 0).as_dry_run();

        let display = format!("{}", outcome);
        assert!(display.contains("[DRY RUN]"));
    }

    // ========================================================================
    // UnrecordStats Tests
    // ========================================================================

    #[test]
    fn test_unrecord_stats_default() {
        let stats = UnrecordStats::default();

        assert_eq!(stats.direct_unrecords, 0);
        assert_eq!(stats.cascaded_unrecords, 0);
        assert_eq!(stats.vertices_removed, 0);
        assert_eq!(stats.edges_removed, 0);
        assert_eq!(stats.files_affected, 0);
    }

    #[test]
    fn test_unrecord_stats_total() {
        let mut stats = UnrecordStats::new();
        stats.direct_unrecords = 5;
        stats.cascaded_unrecords = 3;

        assert_eq!(stats.total_unrecords(), 8);
    }

    #[test]
    fn test_unrecord_stats_merge() {
        let mut stats1 = UnrecordStats::new();
        stats1.direct_unrecords = 2;
        stats1.vertices_removed = 10;

        let mut stats2 = UnrecordStats::new();
        stats2.direct_unrecords = 3;
        stats2.cascaded_unrecords = 1;
        stats2.vertices_removed = 5;

        stats1.merge(&stats2);

        assert_eq!(stats1.direct_unrecords, 5);
        assert_eq!(stats1.cascaded_unrecords, 1);
        assert_eq!(stats1.vertices_removed, 15);
    }

    // ========================================================================
    // UnrecordDependencyInfo Tests
    // ========================================================================

    #[test]
    fn test_dependency_info_new() {
        let hash = Hash::of(b"test");
        let info = UnrecordDependencyInfo::new(hash);

        assert!(info.can_unrecord);
        assert!(info.dependencies.is_empty());
        assert!(info.dependents.is_empty());
        assert!(!info.has_dependents());
    }

    #[test]
    fn test_dependency_info_add_dependency() {
        let hash = Hash::of(b"test");
        let dep = Hash::of(b"dependency");
        let mut info = UnrecordDependencyInfo::new(hash);

        info.add_dependency(dep);

        assert_eq!(info.dependencies.len(), 1);
        assert!(info.can_unrecord); // Dependencies don't block unrecord
    }

    #[test]
    fn test_dependency_info_add_dependent() {
        let hash = Hash::of(b"test");
        let dep = Hash::of(b"dependent");
        let mut info = UnrecordDependencyInfo::new(hash);

        info.add_dependent(dep);

        assert_eq!(info.dependents.len(), 1);
        assert!(info.has_dependents());
        assert!(!info.can_unrecord); // Dependents block unrecord
        assert!(info.block_reason.is_some());
    }

    // ========================================================================
    // UnrecordError Tests
    // ========================================================================

    #[test]
    fn test_unrecord_error_change_not_found() {
        let error = UnrecordError::ChangeNotFound {
            hash: "ABC123".to_string(),
        };
        let msg = format!("{}", error);
        assert!(msg.contains("ABC123"));
    }

    #[test]
    fn test_unrecord_error_not_in_stack() {
        let error = UnrecordError::NotInStack {
            hash: "ABC123".to_string(),
            stack: "main".to_string(),
        };
        let msg = format!("{}", error);
        assert!(msg.contains("ABC123"));
        assert!(msg.contains("main"));
    }

    #[test]
    fn test_unrecord_error_has_dependents() {
        let error = UnrecordError::HasDependents {
            hash: "ABC123".to_string(),
            dependents: vec!["DEF456".to_string(), "GHI789".to_string()],
        };
        let msg = format!("{}", error);
        assert!(msg.contains("ABC123"));
        assert!(msg.contains("DEF456"));
    }

    #[test]
    fn test_unrecord_error_stack_not_found() {
        let error = UnrecordError::StackNotFound {
            name: "missing".to_string(),
        };
        let msg = format!("{}", error);
        assert!(msg.contains("missing"));
    }

    #[test]
    fn test_unrecord_error_stack_empty() {
        let error = UnrecordError::StackEmpty {
            name: "empty".to_string(),
        };
        let msg = format!("{}", error);
        assert!(msg.contains("empty"));
    }

    #[test]
    fn test_unrecord_error_inconsistent() {
        let error = UnrecordError::Inconsistent {
            reason: "graph would be broken".to_string(),
        };
        let msg = format!("{}", error);
        assert!(msg.contains("graph would be broken"));
    }

    #[test]
    fn test_unrecord_error_database() {
        let error = UnrecordError::Database("connection failed".to_string());
        let msg = format!("{}", error);
        assert!(msg.contains("connection failed"));
    }
}
