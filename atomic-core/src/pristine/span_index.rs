//! In-memory per-change vertex-span index.
//!
//! `find_block` / `find_block_end` locate the graph vertex at a position by
//! scanning a change's span range in the GRAPH B-tree. During long passes
//! (applying or assembling a change with thousands of hunks) the scanned
//! change accumulates thousands of spans, so each lookup is O(n) and the whole
//! pass is O(n²).
//!
//! This index loads a change's `(start, end)` spans once into a sorted
//! `BTreeSet` and answers subsequent lookups in O(log n). It is shared by the
//! read-side `CachedGraphTxn` and the write-side `CachedWriteGraphTxn`; each
//! owns the loading (the underlying table handles differ) but both use the
//! query logic here so the resolution semantics stay identical.

use std::collections::{BTreeSet, HashMap};

/// Per-change set of vertex spans, keyed by change id.
#[derive(Default)]
pub(crate) struct VertexSpanIndex {
    spans: HashMap<u64, BTreeSet<(u64, u64)>>,
}

impl VertexSpanIndex {
    /// Whether `change_id`'s spans have already been loaded.
    pub(crate) fn contains_change(&self, change_id: u64) -> bool {
        self.spans.contains_key(&change_id)
    }

    /// Install the full span set for a change (called once after a range scan).
    pub(crate) fn insert_change(&mut self, change_id: u64, set: BTreeSet<(u64, u64)>) {
        self.spans.insert(change_id, set);
    }

    /// Record a newly written span for an already-tracked change.
    ///
    /// Untracked changes are skipped: they will be fully loaded from storage
    /// (which already contains this write) the first time they are queried.
    pub(crate) fn note_write(&mut self, change_id: u64, start: u64, end: u64) {
        if let Some(set) = self.spans.get_mut(&change_id) {
            set.insert((start, end));
        }
    }

    /// Resolve the span containing `target` (preferring a non-empty span over
    /// an empty marker), if this change is loaded.
    pub(crate) fn find_block(&self, change_id: u64, target: u64) -> Option<(u64, u64)> {
        block(self.spans.get(&change_id)?, target)
    }

    /// Resolve the span ending at `target` (or, failing that, containing it),
    /// preferring an empty marker at exactly `target`, if this change is loaded.
    pub(crate) fn find_block_end(&self, change_id: u64, target: u64) -> Option<(u64, u64)> {
        block_end(self.spans.get(&change_id)?, target)
    }
}

/// `find_block` query against a loaded span set.
///
/// Spans within a change do not overlap, so the candidate containing `target`
/// is the greatest-start non-empty span with `start <= target`; if its range
/// doesn't contain `target`, no non-empty span does. Falls back to an empty
/// marker at exactly `target`.
fn block(set: &BTreeSet<(u64, u64)>, target: u64) -> Option<(u64, u64)> {
    for &(s, e) in set.range(..=(target, u64::MAX)).rev() {
        if s == e {
            continue;
        }
        if s <= target && target < e {
            return Some((s, e));
        }
        break;
    }
    if set.contains(&(target, target)) {
        return Some((target, target));
    }
    None
}

/// `find_block_end` query against a loaded span set.
///
/// Prefers an empty marker at exactly `target` (e.g. an inode marker `V[9:9]`
/// over a name vertex `V[0:9]` that also ends at 9), then the non-empty span
/// immediately left of `target` if it ends at or contains `target`, then a
/// non-empty span starting exactly at `target`.
fn block_end(set: &BTreeSet<(u64, u64)>, target: u64) -> Option<(u64, u64)> {
    if set.contains(&(target, target)) {
        return Some((target, target));
    }
    for &(s, e) in set.range(..(target, 0u64)).rev() {
        if s == e {
            continue;
        }
        if e == target || (s <= target && target < e) {
            return Some((s, e));
        }
        break;
    }
    for &(s, e) in set.range((target, 0u64)..=(target, u64::MAX)) {
        if s != e {
            return Some((s, e));
        }
    }
    None
}
