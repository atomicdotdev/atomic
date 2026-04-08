//! State-based content retrieval and change query operations.
//!
//! This module contains operations for querying change history at specific
//! states, retrieving change sets up to a given point, and inspecting
//! which files a change modified. Also contains [`StateBeforeChange`] for
//! representing the state immediately before a change was applied.

use atomic_core::change::Change;
use atomic_core::pristine::{ViewState, ViewTxnT};
use atomic_core::types::{Base32, Hash, Merkle, NodeId};

use super::types::{HistoryEntry, HistoryError, HistoryResult};

// STATE-BASED CONTENT RETRIEVAL

/// Information about the state before a change was applied.
///
/// This struct contains all the information needed to retrieve file content
/// at the state immediately before a specific change was applied to the view.
///
/// # Use Case
///
/// This is primarily used for code review workflows where you want to see
/// what a specific change actually changed:
///
/// ```text
/// Before State (sequence N-1)          After State (sequence N)
/// ┌─────────────────────────┐         ┌─────────────────────────┐
/// │ File content as it was  │  ───▶   │ File content after the  │
/// │ before the change       │ Change  │ change was applied      │
/// └─────────────────────────┘         └─────────────────────────┘
/// ```
///
/// By comparing content at these two states, we can show exactly what
/// the change modified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateBeforeChange {
    /// The sequence number of the parent state (N-1 for a change at sequence N).
    /// This is `None` if the change is at sequence 0 (the first change).
    pub parent_sequence: Option<u64>,

    /// The Merkle state hash before the change was applied.
    /// This is `Merkle::ZERO` if the change is at sequence 0.
    pub parent_state: Merkle,

    /// The sequence number of the change itself.
    pub change_sequence: u64,

    /// The Merkle state hash after the change was applied.
    pub change_state: Merkle,
}

impl StateBeforeChange {
    /// Create a new StateBeforeChange.
    ///
    /// # Arguments
    ///
    /// * `parent_sequence` - Sequence of parent state, or `None` if first change
    /// * `parent_state` - Merkle hash of parent state
    /// * `change_sequence` - Sequence of the change
    /// * `change_state` - Merkle hash after the change
    pub fn new(
        parent_sequence: Option<u64>,
        parent_state: Merkle,
        change_sequence: u64,
        change_state: Merkle,
    ) -> Self {
        Self {
            parent_sequence,
            parent_state,
            change_sequence,
            change_state,
        }
    }

    /// Check if this is the first change in the view.
    ///
    /// If true, the parent state is empty (no content existed before).
    pub fn is_first_change(&self) -> bool {
        self.parent_sequence.is_none()
    }

    /// Get the maximum sequence number to include when retrieving parent state content.
    ///
    /// This returns the exclusive upper bound for change sequences that should
    /// be included when retrieving file content at the parent state.
    ///
    /// # Returns
    ///
    /// - `0` if this is the first change (no changes should be included)
    /// - `parent_sequence + 1` otherwise (includes all changes up to and including parent)
    pub fn parent_max_sequence_exclusive(&self) -> u64 {
        match self.parent_sequence {
            None => 0,
            Some(seq) => seq + 1,
        }
    }
}

impl std::fmt::Display for StateBeforeChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.parent_sequence {
            None => write!(
                f,
                "First change (seq {}) → state {}",
                self.change_sequence,
                &self.change_state.to_base32()[..8]
            ),
            Some(parent_seq) => write!(
                f,
                "State {} (seq {}) → state {} (seq {})",
                &self.parent_state.to_base32()[..8],
                parent_seq,
                &self.change_state.to_base32()[..8],
                self.change_sequence
            ),
        }
    }
}

// QUERY FUNCTIONS

/// Get a specific change by sequence number.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - View to query
/// * `sequence` - Sequence number to retrieve
///
/// # Returns
///
/// The history entry at the given sequence, or an error if out of range.
pub fn get_change_at_sequence<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    sequence: u64,
) -> HistoryResult<HistoryEntry> {
    if sequence >= view.change_count {
        return Err(HistoryError::SequenceOutOfRange {
            sequence,
            max: view.change_count.saturating_sub(1),
        });
    }

    let node_id = txn
        .get_change_at_seq(view, sequence)
        .map_err(|e| HistoryError::Database(e.to_string()))?
        .ok_or_else(|| HistoryError::SequenceOutOfRange {
            sequence,
            max: view.change_count.saturating_sub(1),
        })?;

    let hash = txn
        .get_external(node_id)
        .map_err(|e| HistoryError::Database(e.to_string()))?
        .ok_or_else(|| HistoryError::ChangeNotFound {
            hash: format!("{:?}", node_id),
        })?;

    // Get the Merkle state - we need to iterate to find it
    // This is a bit inefficient but maintains correctness
    let iter = txn
        .iter_changes(view, sequence)
        .map_err(|e| HistoryError::Database(e.to_string()))?;

    for result in iter {
        match result {
            Ok((seq, id, merkle)) if seq == sequence && id == node_id => {
                return Ok(HistoryEntry::new(sequence, node_id, hash, merkle));
            }
            Ok((seq, _, _)) if seq > sequence => break,
            Err(e) => return Err(HistoryError::Database(e.to_string())),
            _ => continue,
        }
    }

    // Fallback with zero merkle if we couldn't find it
    Ok(HistoryEntry::new(sequence, node_id, hash, Merkle::ZERO))
}

/// Find the sequence number for a change by its hash.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - View to search
/// * `hash` - Hash of the change to find
///
/// # Returns
///
/// The sequence number if found, or None if the change is not in the view.
pub fn find_change_sequence<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    hash: &Hash,
) -> HistoryResult<Option<u64>> {
    // First get the internal ID
    let node_id = match txn.get_internal(hash) {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(None),
        Err(e) => return Err(HistoryError::Database(e.to_string())),
    };

    // Then look up the sequence
    match txn.get_change_seq(view, node_id) {
        Ok(seq) => Ok(seq),
        Err(e) => Err(HistoryError::Database(e.to_string())),
    }
}

/// Get the state immediately before a change was applied.
///
/// This function finds the Merkle state of the view as it was just before
/// the specified change was applied. This is essential for showing what
/// a specific change actually modified.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view containing the change
/// * `change_hash` - Hash of the change to find the parent state for
///
/// # Returns
///
/// * `Ok(Some(state))` - The state information before the change
/// * `Ok(None)` - The change is not in this view's history
/// * `Err(_)` - Database error
///
/// # Performance
///
/// This function performs:
/// - One hash-to-internal-ID lookup
/// - One sequence lookup
/// - One or two iterations over the change log (to find parent and current states)
///
/// Total: O(log n) where n is the number of changes in the view.
pub fn get_state_before_change<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    change_hash: &Hash,
) -> HistoryResult<Option<StateBeforeChange>> {
    // First, find the sequence number for this change
    let change_sequence = match find_change_sequence(txn, view, change_hash)? {
        Some(seq) => seq,
        None => return Ok(None), // Change not in this view
    };

    // Get the change's own state (state after it was applied)
    let change_entry = get_change_at_sequence(txn, view, change_sequence)?;
    let change_state = change_entry.state;

    // If this is the first change (sequence 0), there's no parent state
    if change_sequence == 0 {
        return Ok(Some(StateBeforeChange::new(
            None,
            Merkle::ZERO,
            change_sequence,
            change_state,
        )));
    }

    // Get the parent state (state at sequence - 1)
    let parent_sequence = change_sequence - 1;
    let parent_entry = get_change_at_sequence(txn, view, parent_sequence)?;
    let parent_state = parent_entry.state;

    Ok(Some(StateBeforeChange::new(
        Some(parent_sequence),
        parent_state,
        change_sequence,
        change_state,
    )))
}

/// Get all change NodeIds applied up to (but not including) a given sequence.
///
/// This function returns a set of internal NodeIds for all changes that were
/// applied to the view before the specified sequence number. This set can
/// be used to filter graph retrieval to only include content from those changes.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view to query
/// * `max_sequence` - Exclusive upper bound (changes with seq < max_sequence are included)
///
/// # Returns
///
/// A `HashSet<NodeId>` containing all changes applied before `max_sequence`.
/// Returns an empty set if `max_sequence` is 0.
///
/// # Performance
///
/// This function iterates over all changes from sequence 0 to `max_sequence - 1`.
/// Time complexity: O(max_sequence).
///
/// For large repositories, consider caching the result if you need to retrieve
/// content for multiple files at the same state.
pub fn get_changes_up_to_sequence<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    max_sequence: u64,
) -> HistoryResult<std::collections::HashSet<NodeId>> {
    use std::collections::HashSet;

    let mut change_set = HashSet::new();

    // Early return for sequence 0 - no changes to include
    if max_sequence == 0 {
        return Ok(change_set);
    }

    // Iterate from sequence 0 up to (but not including) max_sequence
    let iter = txn
        .iter_changes(view, 0)
        .map_err(|e| HistoryError::Database(e.to_string()))?;

    for result in iter {
        let (seq, node_id, _merkle) = result.map_err(|e| HistoryError::Database(e.to_string()))?;

        // Stop if we've reached or passed max_sequence
        if seq >= max_sequence {
            break;
        }

        change_set.insert(node_id);
    }

    Ok(change_set)
}

/// Get all change NodeIds applied up to and including a specific change.
///
/// This is a convenience wrapper around [`get_changes_up_to_sequence`] that
/// includes the specified change in the result set.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view to query
/// * `change_hash` - Hash of the change to include (and all before it)
///
/// # Returns
///
/// * `Ok(Some(set))` - Set of changes including the specified change and all before it
/// * `Ok(None)` - The change is not in this view's history
/// * `Err(_)` - Database error
pub fn get_changes_up_to_change<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    change_hash: &Hash,
) -> HistoryResult<Option<std::collections::HashSet<NodeId>>> {
    // Find the sequence number for this change
    let sequence = match find_change_sequence(txn, view, change_hash)? {
        Some(seq) => seq,
        None => return Ok(None),
    };

    // Get all changes up to and including this sequence
    let change_set = get_changes_up_to_sequence(txn, view, sequence + 1)?;

    Ok(Some(change_set))
}

/// Get the files modified by a specific change.
///
/// This function returns the paths of all files that were added, modified,
/// or deleted by a specific change. It examines the change's hunks to
/// determine which files were affected.
///
/// # Arguments
///
/// * `change` - The change to examine
///
/// # Returns
///
/// A vector of file paths that were modified by the change.
pub fn get_files_in_change(change: &Change) -> Vec<String> {
    use std::collections::HashSet;

    let mut files: HashSet<String> = HashSet::new();

    for graph_op in change.hunks() {
        if let Some(path) = graph_op.path() {
            files.insert(path.to_string());
        }
    }

    files.into_iter().collect()
}
