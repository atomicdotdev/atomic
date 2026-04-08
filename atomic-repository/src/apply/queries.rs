//! Read-only view query functions for change insertion.
//!
//! These functions query view state without modifying the graph,
//! supporting cross-view insert planning and missing change detection.

use super::{compute_insert_order, InsertError, InsertResult};
use atomic_core::change::Change;
use atomic_core::pristine::{ViewState, ViewTxnT};
use atomic_core::types::Hash;
use std::collections::{HashMap, HashSet};

/// Get all change hashes in a view.
///
/// Returns the hashes in order from oldest (sequence 0) to newest.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view to get changes from
///
/// # Returns
///
/// Ordered vector of (sequence, hash) pairs.
pub fn get_view_changes<T: ViewTxnT>(txn: &T, view: &ViewState) -> InsertResult<Vec<(u64, Hash)>> {
    let mut changes = Vec::new();

    let iter = txn
        .iter_changes(view, 0)
        .map_err(|e| InsertError::Database(e.to_string()))?;

    for result in iter {
        let (seq, node_id, _merkle) = result.map_err(|e| InsertError::Database(e.to_string()))?;

        // Get external hash
        let hash = txn
            .get_external(node_id)
            .map_err(|e| InsertError::Database(e.to_string()))?
            .ok_or_else(|| {
                InsertError::Internal(format!("Change {} has no external hash", node_id.0))
            })?;

        changes.push((seq, hash));
    }

    Ok(changes)
}

/// Get changes that are in the source view but not in the target view.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `from_view` - Source view
/// * `to_view` - Target view
///
/// # Returns
///
/// Vector of hashes that need to be inserted, in dependency order.
pub fn get_missing_changes<T: ViewTxnT>(
    txn: &T,
    from_view: &ViewState,
    to_view: &ViewState,
) -> InsertResult<Vec<Hash>> {
    // Get all changes in source
    let source_changes = get_view_changes(txn, from_view)?;

    // Build set of changes in target
    let target_set: HashSet<Hash> = get_view_changes(txn, to_view)?
        .into_iter()
        .map(|(_, hash)| hash)
        .collect();

    // Filter to changes not in target, preserving order
    let missing: Vec<Hash> = source_changes
        .into_iter()
        .filter(|(_, hash)| !target_set.contains(hash))
        .map(|(_, hash)| hash)
        .collect();

    Ok(missing)
}

/// Get changes up to a specific sequence number.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view to query
/// * `max_sequence` - Maximum sequence (inclusive)
///
/// # Returns
///
/// Vector of hashes up to and including the specified sequence.
pub fn get_changes_up_to_seq<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    max_sequence: u64,
) -> InsertResult<Vec<Hash>> {
    let mut changes = Vec::new();

    let iter = txn
        .iter_changes(view, 0)
        .map_err(|e| InsertError::Database(e.to_string()))?;

    for result in iter {
        let (seq, node_id, _merkle) = result.map_err(|e| InsertError::Database(e.to_string()))?;

        if seq > max_sequence {
            break;
        }

        let hash = txn
            .get_external(node_id)
            .map_err(|e| InsertError::Database(e.to_string()))?
            .ok_or_else(|| {
                InsertError::Internal(format!("Change {} has no external hash", node_id.0))
            })?;

        changes.push(hash);
    }

    Ok(changes)
}

/// Find which changes from a list are missing in a view.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view to check against
/// * `changes` - List of change hashes to check
///
/// # Returns
///
/// Vector of hashes that are not in the view.
pub fn filter_missing_in_view<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    changes: &[Hash],
) -> InsertResult<Vec<Hash>> {
    let mut missing = Vec::new();

    for hash in changes {
        // Get internal ID if it exists
        let internal = txn
            .get_internal(hash)
            .map_err(|e| InsertError::Database(e.to_string()))?;

        if let Some(node_id) = internal {
            // Check if it's in the view
            let in_view = txn
                .get_change_seq(view, node_id)
                .map_err(|e| InsertError::Database(e.to_string()))?
                .is_some();

            if !in_view {
                missing.push(*hash);
            }
        } else {
            // Not even registered, definitely missing
            missing.push(*hash);
        }
    }

    Ok(missing)
}

/// Build a dependency-ordered list of changes to insert.
///
/// Given a set of changes to insert, this function determines the correct
/// order based on their dependencies.
///
/// # Arguments
///
/// * `changes` - Map of hash to Change
///
/// # Returns
///
/// Ordered vector of hashes (dependencies first).
pub fn order_changes_by_deps(changes: &HashMap<Hash, Change>) -> InsertResult<Vec<Hash>> {
    compute_insert_order(changes)
}
