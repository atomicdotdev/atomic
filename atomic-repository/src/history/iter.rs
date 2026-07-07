//! History iteration and log functions.
//!
//! This module contains the [`HistoryIter`] iterator for traversing
//! history entries, and the [`log`] / [`reverse_log`] functions for
//! querying view history.

use atomic_core::pristine::{ViewState, ViewTxnT};
use atomic_core::types::{Hash, Merkle};

use super::types::{HistoryEntry, HistoryError, HistoryOptions, HistoryResult, HistorySummary};

// History Iterator

/// An iterator over history entries.
///
/// This iterator is lazy - it fetches entries as needed from the
/// underlying database cursor.
#[allow(dead_code)]
pub struct HistoryIter<'a, T: ViewTxnT> {
    txn: &'a T,
    view: ViewState,
    inner: Box<
        dyn Iterator<
                Item = Result<
                    (u64, atomic_core::types::NodeId, atomic_core::types::Merkle),
                    atomic_core::pristine::PristineError,
                >,
            > + 'a,
    >,
    limit: Option<usize>,
    count: usize,
    load_headers: bool,
}

impl<'a, T: ViewTxnT> HistoryIter<'a, T> {
    /// Create a new history iterator.
    pub(crate) fn new(
        txn: &'a T,
        view: ViewState,
        inner: Box<
            dyn Iterator<
                    Item = Result<
                        (u64, atomic_core::types::NodeId, atomic_core::types::Merkle),
                        atomic_core::pristine::PristineError,
                    >,
                > + 'a,
        >,
        options: &HistoryOptions,
    ) -> Self {
        Self {
            txn,
            view,
            inner,
            limit: options.limit,
            count: 0,
            load_headers: options.load_headers,
        }
    }

    /// Get the view being iterated.
    pub fn view(&self) -> &ViewState {
        &self.view
    }

    /// Get the number of entries yielded so far.
    pub fn count(&self) -> usize {
        self.count
    }
}

impl<'a, T: ViewTxnT> Iterator for HistoryIter<'a, T> {
    type Item = HistoryResult<HistoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        // Check limit
        if let Some(limit) = self.limit {
            if self.count >= limit {
                return None;
            }
        }

        // Get next from inner iterator
        let result = self.inner.next()?;

        self.count += 1;

        match result {
            Ok((seq, node_id, merkle)) => {
                // Get the external hash
                let hash = match self.txn.get_external(node_id) {
                    Ok(Some(h)) => h,
                    Ok(None) => {
                        return Some(Err(HistoryError::ChangeNotFound {
                            hash: format!("{:?}", node_id),
                        }));
                    }
                    Err(e) => {
                        return Some(Err(HistoryError::Database(e.to_string())));
                    }
                };

                let entry = HistoryEntry::new(seq, node_id, hash, merkle);

                // Note: Header loading would be done by the Repository layer
                // since it requires access to the ChangeStore

                Some(Ok(entry))
            }
            Err(e) => Some(Err(HistoryError::Database(e.to_string()))),
        }
    }
}

// Log Functions

/// Get forward history log from a view.
///
/// Returns an iterator over history entries starting from the given
/// sequence number and proceeding forward (oldest to newest).
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - View to query
/// * `options` - Query options
///
/// # Returns
///
/// An iterator yielding `HistoryEntry` items.
///
/// # Example
///
/// ```rust,ignore
/// let iter = log(&txn, &view, &HistoryOptions::default())?;
/// for entry in iter {
///     let entry = entry?;
///     println!("{}", entry);
/// }
/// ```
pub fn log<'a, T: ViewTxnT>(
    txn: &'a T,
    view: &ViewState,
    options: &HistoryOptions,
) -> HistoryResult<HistoryIter<'a, T>> {
    let inner = txn
        .iter_changes(view, options.from_sequence)
        .map_err(|e| HistoryError::Database(e.to_string()))?;

    Ok(HistoryIter::new(txn, view.clone(), inner, options))
}

/// Get reverse history log from a view.
///
/// Returns entries in reverse order (newest to oldest), starting from
/// either the most recent change or a specified sequence number.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - View to query
/// * `options` - Query options (from_sequence is the upper bound)
///
/// # Returns
///
/// A vector of history entries in reverse order.
///
/// # Note
///
/// This function collects entries into a vector and reverses them,
/// as the underlying iterator only supports forward iteration.
/// For large histories, consider using `log()` with pagination.
pub fn reverse_log<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    options: &HistoryOptions,
) -> HistoryResult<Vec<HistoryEntry>> {
    // Determine the range to query
    let end_seq = if options.from_sequence > 0 {
        options.from_sequence
    } else {
        view.change_count
    };

    let start_seq = if let Some(limit) = options.limit {
        end_seq.saturating_sub(limit as u64)
    } else {
        0
    };

    // Collect forward and reverse
    let forward_options = HistoryOptions {
        from_sequence: start_seq,
        limit: options.limit,
        load_headers: options.load_headers,
        view: options.view.clone(),
        tagged_only: options.tagged_only,
        include_inherited: options.include_inherited,
    };

    let iter = log(txn, view, &forward_options)?;
    let mut entries: Vec<HistoryEntry> = iter.filter_map(|r| r.ok()).collect();
    entries.reverse();

    Ok(entries)
}

/// Get a summary of the view's history.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - View to summarize
///
/// # Returns
///
/// A `HistorySummary` with aggregate statistics.
pub fn history_summary<T: ViewTxnT>(txn: &T, view: &ViewState) -> HistoryResult<HistorySummary> {
    let mut summary = HistorySummary::new(&view.name, view);

    // Get first change
    if view.change_count > 0 {
        if let Ok(Some(first_id)) = txn.get_change_at_seq(view, 0) {
            if let Ok(Some(hash)) = txn.get_external(first_id) {
                summary.first_change = Some(hash);
            }
        }

        // Get last change
        if let Ok(Some(last_id)) = txn.get_change_at_seq(view, view.change_count - 1) {
            if let Ok(Some(hash)) = txn.get_external(last_id) {
                summary.last_change = Some(hash);
            }
        }
    }

    // Note: Tagged count would require iterating through the history
    // or maintaining a separate counter. For now, we leave it at 0.

    Ok(summary)
}

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

/// Check if a change is in the view's history.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - View to search
/// * `hash` - Hash of the change to check
///
/// # Returns
///
/// `true` if the change is in the view's history.
pub fn is_change_in_history<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    hash: &Hash,
) -> HistoryResult<bool> {
    find_change_sequence(txn, view, hash).map(|seq| seq.is_some())
}
