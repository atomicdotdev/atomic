//! CRDT ordering and insertion position resolution.
//!
//! This module provides utilities for maintaining deterministic ordering
//! of CRDT elements. In a CRDT system, concurrent insertions at the same
//! position must be ordered deterministically to ensure all replicas
//! converge to the same state.
//!
//! # Ordering Principles
//!
//! 1. **ID-Based Ordering**: Elements are ordered by their CRDT IDs.
//!    IDs embed the creating change, providing global uniqueness and
//!    deterministic ordering.
//!
//! 2. **Insertion Point Resolution**: When inserting "after" a reference,
//!    the actual position is determined by finding all elements that
//!    also claim to be "after" that reference, then ordering by ID.
//!
//! 3. **Tombstone Preservation**: Deleted elements retain their position
//!    in the ordering to maintain consistency.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::crdt::apply::order::{find_insert_position, InsertPosition};
//! use atomic_core::crdt::BranchId;
//! use atomic_core::types::NodeId;
//!
//! // Simulate finding where to insert a new branch
//! let new_id = BranchId::new(NodeId::new(3), 0);
//!
//! // Existing elements that are also "after" the same reference
//! let concurrent = vec![
//!     BranchId::new(NodeId::new(2), 0), // Also inserted after (1, 0)
//!     BranchId::new(NodeId::new(4), 0), // Also inserted after (1, 0)
//! ];
//!
//! // New element (3, 0) goes between (2, 0) and (4, 0) based on ID ordering
//! let position = find_insert_position(&new_id, &concurrent);
//! assert_eq!(position, InsertPosition::After(0)); // After index 0 (the element with ID 2)
//! ```

use crate::crdt::{BranchId, LeafId};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

// =============================================================================
// InsertPosition
// =============================================================================

/// The resolved position for an insertion operation.
///
/// After resolving concurrent insertions, this indicates where in an
/// ordered sequence the new element should be placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPosition {
    /// Insert at the beginning of the sequence.
    Start,

    /// Insert after the element at the given index.
    After(usize),

    /// Insert at the end of the sequence.
    End,
}

impl InsertPosition {
    /// Returns the index at which to insert in a Vec-like structure.
    ///
    /// For use with `Vec::insert()`.
    #[inline]
    pub fn to_insert_index(&self, len: usize) -> usize {
        match self {
            InsertPosition::Start => 0,
            InsertPosition::After(idx) => idx + 1,
            InsertPosition::End => len,
        }
    }

    /// Returns `true` if this is an insertion at the start.
    #[inline]
    pub fn is_start(&self) -> bool {
        matches!(self, InsertPosition::Start)
    }

    /// Returns `true` if this is an insertion at the end.
    #[inline]
    pub fn is_end(&self) -> bool {
        matches!(self, InsertPosition::End)
    }
}

impl fmt::Display for InsertPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InsertPosition::Start => write!(f, "start"),
            InsertPosition::After(idx) => write!(f, "after index {}", idx),
            InsertPosition::End => write!(f, "end"),
        }
    }
}

// =============================================================================
// CrdtOrdering Trait
// =============================================================================

/// Trait for CRDT elements that can be ordered.
///
/// Implementors must provide a way to extract an ordering key that
/// enables deterministic comparison between elements.
pub trait CrdtOrdering: Ord + Clone {
    /// The type of the "after" reference used for insertion.
    type AfterRef: Eq + Clone;

    /// Returns the reference point this element was inserted after.
    ///
    /// Returns `None` if inserted at the start.
    fn inserted_after(&self) -> Option<Self::AfterRef>;

    /// Returns `true` if this element should come before `other`
    /// when both are inserted after the same reference.
    ///
    /// Default implementation uses the `Ord` trait.
    fn precedes(&self, other: &Self) -> bool {
        self < other
    }
}

// =============================================================================
// Insertion Position Resolution
// =============================================================================

/// Finds the insertion position for a new element among concurrent insertions.
///
/// Given a new element ID and a list of existing elements that were all
/// inserted after the same reference point, determines where the new
/// element should be placed based on ID ordering.
///
/// # Arguments
///
/// * `new_id` - The ID of the element being inserted
/// * `concurrent` - Existing elements that share the same insertion point
///
/// # Returns
///
/// The position where the new element should be inserted.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::order::{find_insert_position, InsertPosition};
/// use atomic_core::crdt::BranchId;
/// use atomic_core::types::NodeId;
///
/// let new_id = BranchId::new(NodeId::new(3), 0);
/// let concurrent = vec![
///     BranchId::new(NodeId::new(2), 0),
///     BranchId::new(NodeId::new(5), 0),
/// ];
///
/// // (3, 0) comes after (2, 0) but before (5, 0)
/// let pos = find_insert_position(&new_id, &concurrent);
/// assert_eq!(pos, InsertPosition::After(0));
/// ```
pub fn find_insert_position<T: Ord>(new_id: &T, concurrent: &[T]) -> InsertPosition {
    if concurrent.is_empty() {
        return InsertPosition::Start;
    }

    // Find where the new ID fits in the sorted order
    match concurrent.binary_search(new_id) {
        // Exact match (shouldn't happen with unique IDs)
        Ok(idx) => InsertPosition::After(idx),
        // Not found, idx is where it would be inserted
        Err(idx) => {
            if idx == 0 {
                InsertPosition::Start
            } else if idx >= concurrent.len() {
                InsertPosition::End
            } else {
                InsertPosition::After(idx - 1)
            }
        }
    }
}

/// Finds the insertion position for a branch among existing branches.
///
/// This is a convenience wrapper for [`find_insert_position`] specialized
/// for branch IDs.
#[inline]
pub fn find_branch_insert_position(
    new_id: &BranchId,
    concurrent: &[BranchId],
) -> InsertPosition {
    find_insert_position(new_id, concurrent)
}

/// Finds the insertion position for a leaf among existing leaves.
///
/// This is a convenience wrapper for [`find_insert_position`] specialized
/// for leaf IDs.
#[inline]
pub fn find_leaf_insert_position(new_id: &LeafId, concurrent: &[LeafId]) -> InsertPosition {
    find_insert_position(new_id, concurrent)
}

// =============================================================================
// OrderingEntry
// =============================================================================

/// An entry in an ordering sequence with its insertion reference.
///
/// Tracks both the element ID and the reference point it was inserted after,
/// enabling reconstruction of the ordering from operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderingEntry<Id: Clone + Eq> {
    /// The element's ID.
    id: Id,

    /// The ID this element was inserted after, or `None` for start.
    after: Option<Id>,
}

impl<Id: Clone + Eq> OrderingEntry<Id> {
    /// Creates a new ordering entry.
    #[inline]
    pub fn new(id: Id, after: Option<Id>) -> Self {
        Self { id, after }
    }

    /// Returns the element's ID.
    #[inline]
    pub fn id(&self) -> &Id {
        &self.id
    }

    /// Returns the reference this element was inserted after.
    #[inline]
    pub fn after(&self) -> Option<&Id> {
        self.after.as_ref()
    }

    /// Returns `true` if this element was inserted at the start.
    #[inline]
    pub fn is_at_start(&self) -> bool {
        self.after.is_none()
    }
}

impl<Id: Clone + Eq + Ord> Ord for OrderingEntry<Id> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl<Id: Clone + Eq + Ord> PartialOrd for OrderingEntry<Id> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// =============================================================================
// OrderingSequence
// =============================================================================

/// A sequence of elements with CRDT ordering.
///
/// Maintains elements in their proper order, resolving concurrent insertions
/// using ID-based ordering.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::order::OrderingSequence;
/// use atomic_core::crdt::BranchId;
/// use atomic_core::types::NodeId;
///
/// let mut seq: OrderingSequence<BranchId> = OrderingSequence::new();
///
/// // Insert at start
/// let id1 = BranchId::new(NodeId::new(1), 0);
/// seq.insert(id1, None);
///
/// // Insert after id1
/// let id2 = BranchId::new(NodeId::new(2), 0);
/// seq.insert(id2, Some(id1));
///
/// assert_eq!(seq.len(), 2);
/// assert_eq!(seq.get(0), Some(&id1));
/// assert_eq!(seq.get(1), Some(&id2));
/// ```
#[derive(Debug, Clone)]
pub struct OrderingSequence<Id: Clone + Eq + Ord> {
    /// The ordered elements.
    elements: Vec<Id>,

    /// The "after" reference for each element (parallel to elements).
    after_refs: Vec<Option<Id>>,
}

impl<Id: Clone + Eq + Ord> Default for OrderingSequence<Id> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Id: Clone + Eq + Ord> OrderingSequence<Id> {
    /// Creates a new empty sequence.
    #[inline]
    pub fn new() -> Self {
        Self {
            elements: Vec::new(),
            after_refs: Vec::new(),
        }
    }

    /// Creates a sequence with pre-allocated capacity.
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            elements: Vec::with_capacity(capacity),
            after_refs: Vec::with_capacity(capacity),
        }
    }

    /// Returns the number of elements.
    #[inline]
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    /// Returns `true` if the sequence is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Returns the element at the given index.
    #[inline]
    pub fn get(&self, index: usize) -> Option<&Id> {
        self.elements.get(index)
    }

    /// Returns the "after" reference for the element at the given index.
    #[inline]
    pub fn get_after(&self, index: usize) -> Option<Option<&Id>> {
        self.after_refs.get(index).map(|r| r.as_ref())
    }

    /// Returns an iterator over the elements.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &Id> {
        self.elements.iter()
    }

    /// Returns the elements as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[Id] {
        &self.elements
    }

    /// Inserts an element at its correct position.
    ///
    /// # Arguments
    ///
    /// * `id` - The ID of the element to insert
    /// * `after` - The ID this element should be inserted after, or `None` for start
    ///
    /// # Returns
    ///
    /// The index at which the element was inserted.
    pub fn insert(&mut self, id: Id, after: Option<Id>) -> usize {
        let position = self.resolve_position(&id, &after);
        let index = position.to_insert_index(self.elements.len());

        self.elements.insert(index, id);
        self.after_refs.insert(index, after);

        index
    }

    /// Resolves the insertion position for a new element.
    ///
    /// Finds all existing elements that share the same "after" reference
    /// and determines where the new element fits based on ID ordering.
    fn resolve_position(&self, id: &Id, after: &Option<Id>) -> InsertPosition {
        // Find the starting point: either the referenced element or the start
        let start_index = match after {
            Some(ref after_id) => {
                // Find the element we're inserting after
                match self.elements.iter().position(|e| e == after_id) {
                    Some(idx) => idx + 1,
                    None => 0, // Reference not found, treat as start
                }
            }
            None => 0,
        };

        // Find all concurrent insertions (elements with the same "after" reference)
        let mut concurrent: Vec<&Id> = Vec::new();
        for i in start_index..self.elements.len() {
            if self.after_refs[i] == *after {
                concurrent.push(&self.elements[i]);
            } else {
                // Once we see a different "after" reference, we've passed the concurrent group
                break;
            }
        }

        if concurrent.is_empty() {
            if start_index == 0 {
                InsertPosition::Start
            } else {
                InsertPosition::After(start_index - 1)
            }
        } else {
            // Find where the new ID fits among concurrent insertions
            match concurrent.binary_search(&id) {
                Ok(_) => {
                    // Exact match (duplicate ID) - shouldn't happen
                    InsertPosition::After(start_index + concurrent.len() - 1)
                }
                Err(pos) => {
                    if pos == 0 {
                        if start_index == 0 {
                            InsertPosition::Start
                        } else {
                            InsertPosition::After(start_index - 1)
                        }
                    } else {
                        InsertPosition::After(start_index + pos - 1)
                    }
                }
            }
        }
    }

    /// Checks if the sequence contains an element with the given ID.
    #[inline]
    pub fn contains(&self, id: &Id) -> bool {
        self.elements.contains(id)
    }

    /// Finds the index of an element by ID.
    #[inline]
    pub fn position(&self, id: &Id) -> Option<usize> {
        self.elements.iter().position(|e| e == id)
    }

    /// Removes an element by ID.
    ///
    /// Returns the removed element if found.
    pub fn remove(&mut self, id: &Id) -> Option<Id> {
        if let Some(idx) = self.position(id) {
            self.after_refs.remove(idx);
            Some(self.elements.remove(idx))
        } else {
            None
        }
    }

    /// Clears all elements from the sequence.
    #[inline]
    pub fn clear(&mut self) {
        self.elements.clear();
        self.after_refs.clear();
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    // =========================================================================
    // InsertPosition Tests
    // =========================================================================

    #[test]
    fn test_insert_position_to_index() {
        assert_eq!(InsertPosition::Start.to_insert_index(5), 0);
        assert_eq!(InsertPosition::After(0).to_insert_index(5), 1);
        assert_eq!(InsertPosition::After(2).to_insert_index(5), 3);
        assert_eq!(InsertPosition::End.to_insert_index(5), 5);
    }

    #[test]
    fn test_insert_position_is_start() {
        assert!(InsertPosition::Start.is_start());
        assert!(!InsertPosition::After(0).is_start());
        assert!(!InsertPosition::End.is_start());
    }

    #[test]
    fn test_insert_position_is_end() {
        assert!(!InsertPosition::Start.is_end());
        assert!(!InsertPosition::After(0).is_end());
        assert!(InsertPosition::End.is_end());
    }

    #[test]
    fn test_insert_position_display() {
        assert!(InsertPosition::Start.to_string().contains("start"));
        assert!(InsertPosition::After(5).to_string().contains("5"));
        assert!(InsertPosition::End.to_string().contains("end"));
    }

    // =========================================================================
    // find_insert_position Tests
    // =========================================================================

    #[test]
    fn test_find_position_empty() {
        let concurrent: Vec<i32> = vec![];
        assert_eq!(find_insert_position(&5, &concurrent), InsertPosition::Start);
    }

    #[test]
    fn test_find_position_before_all() {
        let concurrent = vec![5, 10, 15];
        assert_eq!(find_insert_position(&2, &concurrent), InsertPosition::Start);
    }

    #[test]
    fn test_find_position_after_all() {
        let concurrent = vec![5, 10, 15];
        assert_eq!(find_insert_position(&20, &concurrent), InsertPosition::End);
    }

    #[test]
    fn test_find_position_middle() {
        let concurrent = vec![5, 10, 15];
        assert_eq!(find_insert_position(&7, &concurrent), InsertPosition::After(0));
        assert_eq!(find_insert_position(&12, &concurrent), InsertPosition::After(1));
    }

    #[test]
    fn test_find_position_branch_ids() {
        let id1 = BranchId::new(NodeId::new(1), 0);
        let id2 = BranchId::new(NodeId::new(2), 0);
        let id3 = BranchId::new(NodeId::new(3), 0);

        let concurrent = vec![id1, id3];
        let pos = find_branch_insert_position(&id2, &concurrent);

        // id2 should go after id1 (index 0)
        assert_eq!(pos, InsertPosition::After(0));
    }

    #[test]
    fn test_find_position_leaf_ids() {
        let id1 = LeafId::new(NodeId::new(1), 0);
        let id2 = LeafId::new(NodeId::new(2), 0);
        let id3 = LeafId::new(NodeId::new(3), 0);

        let concurrent = vec![id1, id3];
        let pos = find_leaf_insert_position(&id2, &concurrent);

        // id2 should go after id1 (index 0)
        assert_eq!(pos, InsertPosition::After(0));
    }

    // =========================================================================
    // OrderingEntry Tests
    // =========================================================================

    #[test]
    fn test_ordering_entry_new() {
        let entry: OrderingEntry<i32> = OrderingEntry::new(5, Some(3));
        assert_eq!(entry.id(), &5);
        assert_eq!(entry.after(), Some(&3));
    }

    #[test]
    fn test_ordering_entry_at_start() {
        let entry: OrderingEntry<i32> = OrderingEntry::new(1, None);
        assert!(entry.is_at_start());
        assert!(entry.after().is_none());
    }

    #[test]
    fn test_ordering_entry_not_at_start() {
        let entry: OrderingEntry<i32> = OrderingEntry::new(5, Some(1));
        assert!(!entry.is_at_start());
    }

    #[test]
    fn test_ordering_entry_ord() {
        let entry1: OrderingEntry<i32> = OrderingEntry::new(1, None);
        let entry2: OrderingEntry<i32> = OrderingEntry::new(5, None);
        let entry3: OrderingEntry<i32> = OrderingEntry::new(5, Some(1));

        // Ord comparison is by ID only
        assert!(entry1 < entry2);
        assert!(entry2.cmp(&entry3) == std::cmp::Ordering::Equal);

        // But PartialEq compares all fields (derived)
        assert!(entry2 != entry3); // Different "after" references
    }

    #[test]
    fn test_ordering_entry_serde() {
        let entry: OrderingEntry<i32> = OrderingEntry::new(5, Some(3));
        let json = serde_json::to_string(&entry).unwrap();
        let restored: OrderingEntry<i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, restored);
    }

    // =========================================================================
    // OrderingSequence Tests
    // =========================================================================

    #[test]
    fn test_sequence_new() {
        let seq: OrderingSequence<i32> = OrderingSequence::new();
        assert!(seq.is_empty());
        assert_eq!(seq.len(), 0);
    }

    #[test]
    fn test_sequence_with_capacity() {
        let seq: OrderingSequence<i32> = OrderingSequence::with_capacity(10);
        assert!(seq.is_empty());
    }

    #[test]
    fn test_sequence_insert_at_start() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        let idx = seq.insert(5, None);

        assert_eq!(idx, 0);
        assert_eq!(seq.len(), 1);
        assert_eq!(seq.get(0), Some(&5));
    }

    #[test]
    fn test_sequence_insert_after() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(1, None);
        let idx = seq.insert(2, Some(1));

        assert_eq!(idx, 1);
        assert_eq!(seq.len(), 2);
        assert_eq!(seq.get(0), Some(&1));
        assert_eq!(seq.get(1), Some(&2));
    }

    #[test]
    fn test_sequence_concurrent_insert() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();

        // Both 2 and 3 are inserted after None (start)
        seq.insert(3, None);
        seq.insert(2, None); // Should go before 3

        assert_eq!(seq.get(0), Some(&2));
        assert_eq!(seq.get(1), Some(&3));
    }

    #[test]
    fn test_sequence_branch_ids() {
        let mut seq: OrderingSequence<BranchId> = OrderingSequence::new();

        let id1 = BranchId::new(NodeId::new(1), 0);
        let id2 = BranchId::new(NodeId::new(2), 0);
        let id3 = BranchId::new(NodeId::new(3), 0);

        seq.insert(id1, None);
        seq.insert(id3, Some(id1));
        seq.insert(id2, Some(id1)); // Goes between id1 and id3

        assert_eq!(seq.get(0), Some(&id1));
        assert_eq!(seq.get(1), Some(&id2));
        assert_eq!(seq.get(2), Some(&id3));
    }

    #[test]
    fn test_sequence_get_after() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(1, None);
        seq.insert(2, Some(1));

        assert_eq!(seq.get_after(0), Some(None));
        assert_eq!(seq.get_after(1), Some(Some(&1)));
    }

    #[test]
    fn test_sequence_iter() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(1, None);
        seq.insert(2, Some(1));

        let elements: Vec<_> = seq.iter().cloned().collect();
        assert_eq!(elements, vec![1, 2]);
    }

    #[test]
    fn test_sequence_contains() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(5, None);

        assert!(seq.contains(&5));
        assert!(!seq.contains(&10));
    }

    #[test]
    fn test_sequence_position() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(1, None);
        seq.insert(2, Some(1));

        assert_eq!(seq.position(&1), Some(0));
        assert_eq!(seq.position(&2), Some(1));
        assert_eq!(seq.position(&99), None);
    }

    #[test]
    fn test_sequence_remove() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(1, None);
        seq.insert(2, Some(1));

        let removed = seq.remove(&1);
        assert_eq!(removed, Some(1));
        assert_eq!(seq.len(), 1);
        assert_eq!(seq.get(0), Some(&2));
    }

    #[test]
    fn test_sequence_remove_not_found() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(1, None);

        let removed = seq.remove(&99);
        assert_eq!(removed, None);
        assert_eq!(seq.len(), 1);
    }

    #[test]
    fn test_sequence_clear() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(1, None);
        seq.insert(2, Some(1));

        seq.clear();
        assert!(seq.is_empty());
    }

    #[test]
    fn test_sequence_default() {
        let seq: OrderingSequence<i32> = OrderingSequence::default();
        assert!(seq.is_empty());
    }

    #[test]
    fn test_sequence_clone() {
        let mut seq: OrderingSequence<i32> = OrderingSequence::new();
        seq.insert(1, None);
        seq.insert(2, Some(1));

        let cloned = seq.clone();
        assert_eq!(cloned.len(), seq.len());
        assert_eq!(cloned.get(0), seq.get(0));
        assert_eq!(cloned.get(1), seq.get(1));
    }

    #[test]
    fn test_sequence_complex_ordering() {
        let mut seq: OrderingSequence<BranchId> = OrderingSequence::new();

        // Simulate a more complex scenario:
        // - Line 1 is added at start
        // - Lines 2 and 4 are concurrently added after line 1
        // - Line 3 is added after line 2

        let l1 = BranchId::new(NodeId::new(1), 0);
        let l2 = BranchId::new(NodeId::new(2), 0);
        let l3 = BranchId::new(NodeId::new(3), 0);
        let l4 = BranchId::new(NodeId::new(4), 0);

        seq.insert(l1, None);
        seq.insert(l4, Some(l1)); // First concurrent insert
        seq.insert(l2, Some(l1)); // Second concurrent insert (goes before l4)
        seq.insert(l3, Some(l2)); // After l2

        // Expected order: l1, l2, l3, l4
        assert_eq!(seq.get(0), Some(&l1));
        assert_eq!(seq.get(1), Some(&l2));
        assert_eq!(seq.get(2), Some(&l3));
        assert_eq!(seq.get(3), Some(&l4));
    }
}
