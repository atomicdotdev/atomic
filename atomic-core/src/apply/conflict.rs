//! Conflict detection and resolution tracking
//!
//! This module handles detecting and tracking conflicts that arise during
//! change application. Conflicts occur when concurrent changes modify the
//! same content in incompatible ways.
//!
//! # Overview
//!
//! In a distributed VCS, conflicts are inevitable when multiple people edit
//! the same content concurrently. Atomic handles this by:
//!
//! 1. **Detecting conflicts** during change application
//! 2. **Marking conflicting regions** in the graph with special edges
//! 3. **Tracking conflicts** in the workspace for later resolution
//! 4. **Allowing output** with conflict markers for human resolution
//!
//! # Conflict Types
//!
//! ## Zombie Conflicts
//!
//! A zombie conflict occurs when:
//! - Change A deletes content
//! - Change B modifies or extends that same content
//! - Neither change knows about the other
//!
//! The "zombie" is the deleted content that still has live connections
//! from the unknown change.
//!
//! ```text
//! Change A: Delete "Hello"
//! Change B: Change "Hello" to "Hello World"
//!
//! Result: "Hello" is a zombie - deleted by A, but modified by B
//! ```
//!
//! ## Missing Context Conflicts
//!
//! A missing context conflict occurs when:
//! - A change references context that doesn't exist yet
//! - Usually during partial application or out-of-order application
//!
//! ## Order Conflicts
//!
//! An order conflict occurs when:
//! - Two changes insert content at the same position
//! - The relative order is ambiguous
//!
//! # Conflict Tracking
//!
//! The [`ConflictTracker`] collects all conflicts during application:
//!
//! ```rust,ignore
//! let mut tracker = ConflictTracker::new();
//!
//! // During application, conflicts are recorded
//! tracker.add_zombie(ZombieConflict::new(span, change_id));
//!
//! // After application, check for conflicts
//! if tracker.has_conflicts() {
//!     for zombie in tracker.zombies() {
//!         println!("Zombie at {:?}", zombie.node);
//!     }
//! }
//! ```
//!
//! # Resolution
//!
//! Conflicts are resolved by creating a new change that:
//! - Explicitly depends on both conflicting changes
//! - Chooses one resolution or creates a merge
//!
//! This is typically done interactively by the user.

use crate::types::{Hash, NodeId, Position, GraphNode};
use std::collections::HashSet;

// =============================================================================
// Zombie Conflicts
// =============================================================================

/// A zombie conflict represents deleted content with live connections.
///
/// Zombies occur when:
/// - One change deletes content
/// - Another change adds edges to/from that content
/// - Neither change knows about the other
///
/// # Fields
///
/// - `span`: The span that is a zombie (deleted but with live edges)
/// - `deleted_by`: Change(s) that deleted this content
/// - `connected_by`: Change(s) that added live edges
/// - `inode`: The file containing this zombie (if known)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZombieConflict {
    /// The zombie span
    pub node: GraphNode<NodeId>,
    /// Changes that deleted this span
    pub deleted_by: Vec<NodeId>,
    /// Changes that added live connections
    pub connected_by: Vec<NodeId>,
    /// File inode (if known)
    pub inode: Option<Position<NodeId>>,
}

impl ZombieConflict {
    /// Create a new zombie conflict.
    ///
    /// # Arguments
    ///
    /// * `span` - The zombie span
    pub fn new(node: GraphNode<NodeId>) -> Self {
        Self {
            node,
            deleted_by: Vec::new(),
            connected_by: Vec::new(),
            inode: None,
        }
    }

    /// Create a zombie conflict with the deleting change.
    pub fn deleted_by(node: GraphNode<NodeId>, change: NodeId) -> Self {
        Self {
            node,
            deleted_by: vec![change],
            connected_by: Vec::new(),
            inode: None,
        }
    }

    /// Add a change that deleted this span.
    pub fn add_deleted_by(&mut self, change: NodeId) {
        if !self.deleted_by.contains(&change) {
            self.deleted_by.push(change);
        }
    }

    /// Add a change that connected to this span.
    pub fn add_connected_by(&mut self, change: NodeId) {
        if !self.connected_by.contains(&change) {
            self.connected_by.push(change);
        }
    }

    /// Set the file inode.
    pub fn with_inode(mut self, inode: Position<NodeId>) -> Self {
        self.inode = Some(inode);
        self
    }

    /// Check if this zombie has been resolved.
    ///
    /// A zombie is resolved when there are no more live connections
    /// from unknown changes.
    pub fn is_resolved(&self) -> bool {
        self.connected_by.is_empty()
    }
}

// =============================================================================
// Missing Context Conflicts
// =============================================================================

/// A missing context conflict represents a reference to non-existent context.
///
/// This occurs when:
/// - A change references an predecessors or successors that doesn't exist
/// - Usually because the change it depends on hasn't been applied yet
///
/// # Fields
///
/// - `position`: The position that was referenced but not found
/// - `is_predecessor`: Whether this was an predecessors (vs successors)
/// - `during_change`: The change that had the missing context
/// - `expected_change`: The change hash that was expected
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingContextConflict {
    /// The position that was referenced
    pub position: Position<NodeId>,
    /// Whether this was an predecessors reference
    pub is_predecessor: bool,
    /// The change being applied when this occurred
    pub during_change: NodeId,
    /// The expected change hash (if known)
    pub expected_change: Option<Hash>,
}

impl MissingContextConflict {
    /// Create a new missing context conflict.
    pub fn new(position: Position<NodeId>, is_predecessor: bool, during_change: NodeId) -> Self {
        Self {
            position,
            is_predecessor,
            during_change,
            expected_change: None,
        }
    }

    /// Create for an predecessors reference.
    pub fn predecessors(position: Position<NodeId>, during_change: NodeId) -> Self {
        Self::new(position, true, during_change)
    }

    /// Create for a successors reference.
    pub fn successors(position: Position<NodeId>, during_change: NodeId) -> Self {
        Self::new(position, false, during_change)
    }

    /// Set the expected change hash.
    pub fn with_expected(mut self, hash: Hash) -> Self {
        self.expected_change = Some(hash);
        self
    }
}

// =============================================================================
// Order Conflicts
// =============================================================================

/// An order conflict represents ambiguous insertion order.
///
/// This occurs when:
/// - Two changes insert content at the same position
/// - There's no dependency between them to determine order
///
/// # Fields
///
/// - `position`: The position where both changes insert
/// - `vertices`: The vertices that were inserted (from different changes)
/// - `changes`: The changes that caused the conflict
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderConflict {
    /// The position where insertions conflict
    pub position: Position<NodeId>,
    /// Vertices inserted at this position
    pub vertices: Vec<GraphNode<NodeId>>,
    /// Changes that inserted at this position
    pub changes: Vec<NodeId>,
}

impl OrderConflict {
    /// Create a new order conflict.
    pub fn new(position: Position<NodeId>) -> Self {
        Self {
            position,
            vertices: Vec::new(),
            changes: Vec::new(),
        }
    }

    /// Add an insertion to this conflict.
    pub fn add_insertion(&mut self, node: GraphNode<NodeId>, change: NodeId) {
        if !self.vertices.contains(&node) {
            self.vertices.push(node);
        }
        if !self.changes.contains(&change) {
            self.changes.push(change);
        }
    }

    /// Get the number of conflicting insertions.
    pub fn conflict_count(&self) -> usize {
        self.vertices.len()
    }
}

// =============================================================================
// Conflict Tracker
// =============================================================================

/// Tracks all conflicts detected during change application.
///
/// The conflict tracker collects conflicts as they're detected and provides
/// methods for querying and resolving them.
///
/// # Example
///
/// ```rust
/// use atomic_core::apply::conflict::ConflictTracker;
///
/// let mut tracker = ConflictTracker::new();
///
/// // Check if any conflicts exist
/// if tracker.has_conflicts() {
///     println!("Found {} zombies", tracker.zombie_count());
/// }
/// ```
#[derive(Debug, Default)]
pub struct ConflictTracker {
    /// Zombie conflicts
    zombies: Vec<ZombieConflict>,
    /// Missing context conflicts
    missing_contexts: Vec<MissingContextConflict>,
    /// Order conflicts
    order_conflicts: Vec<OrderConflict>,
    /// Set of all involved changes
    involved_changes: HashSet<NodeId>,
}

impl ConflictTracker {
    /// Create a new empty conflict tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all tracked conflicts.
    pub fn clear(&mut self) {
        self.zombies.clear();
        self.missing_contexts.clear();
        self.order_conflicts.clear();
        self.involved_changes.clear();
    }

    // =========================================================================
    // Zombie Conflicts
    // =========================================================================

    /// Add a zombie conflict.
    pub fn add_zombie(&mut self, zombie: ZombieConflict) {
        for change in &zombie.deleted_by {
            self.involved_changes.insert(*change);
        }
        for change in &zombie.connected_by {
            self.involved_changes.insert(*change);
        }
        self.zombies.push(zombie);
    }

    /// Add a zombie span (simplified API).
    pub fn add_zombie_vertex(&mut self, node: GraphNode<NodeId>) {
        self.zombies.push(ZombieConflict::new(node));
    }

    /// Get all zombie conflicts.
    pub fn zombies(&self) -> &[ZombieConflict] {
        &self.zombies
    }

    /// Get the number of zombie conflicts.
    pub fn zombie_count(&self) -> usize {
        self.zombies.len()
    }

    /// Check if there are any zombie conflicts.
    pub fn has_zombies(&self) -> bool {
        !self.zombies.is_empty()
    }

    // =========================================================================
    // Missing Context Conflicts
    // =========================================================================

    /// Add a missing context conflict.
    pub fn add_missing_context(&mut self, conflict: MissingContextConflict) {
        self.involved_changes.insert(conflict.during_change);
        self.missing_contexts.push(conflict);
    }

    /// Get all missing context conflicts.
    pub fn missing_contexts(&self) -> &[MissingContextConflict] {
        &self.missing_contexts
    }

    /// Get the number of missing context conflicts.
    pub fn missing_context_count(&self) -> usize {
        self.missing_contexts.len()
    }

    /// Check if there are any missing context conflicts.
    pub fn has_missing_contexts(&self) -> bool {
        !self.missing_contexts.is_empty()
    }

    // =========================================================================
    // Order Conflicts
    // =========================================================================

    /// Add an order conflict.
    pub fn add_order_conflict(&mut self, conflict: OrderConflict) {
        for change in &conflict.changes {
            self.involved_changes.insert(*change);
        }
        self.order_conflicts.push(conflict);
    }

    /// Get all order conflicts.
    pub fn order_conflicts(&self) -> &[OrderConflict] {
        &self.order_conflicts
    }

    /// Get the number of order conflicts.
    pub fn order_conflict_count(&self) -> usize {
        self.order_conflicts.len()
    }

    /// Check if there are any order conflicts.
    pub fn has_order_conflicts(&self) -> bool {
        !self.order_conflicts.is_empty()
    }

    // =========================================================================
    // Aggregate Queries
    // =========================================================================

    /// Check if there are any conflicts of any type.
    pub fn has_conflicts(&self) -> bool {
        self.has_zombies() || self.has_missing_contexts() || self.has_order_conflicts()
    }

    /// Get the total number of conflicts.
    pub fn total_conflict_count(&self) -> usize {
        self.zombie_count() + self.missing_context_count() + self.order_conflict_count()
    }

    /// Check if the tracker is empty.
    pub fn is_empty(&self) -> bool {
        !self.has_conflicts()
    }

    /// Get all changes involved in conflicts.
    pub fn involved_changes(&self) -> impl Iterator<Item = &NodeId> {
        self.involved_changes.iter()
    }

    /// Get the number of changes involved in conflicts.
    pub fn involved_change_count(&self) -> usize {
        self.involved_changes.len()
    }
}

// =============================================================================
// Conflict Summary
// =============================================================================

/// Summary statistics for conflicts.
#[derive(Debug, Clone, Default)]
pub struct ConflictSummary {
    /// Number of zombie conflicts
    pub zombie_count: usize,
    /// Number of missing context conflicts
    pub missing_context_count: usize,
    /// Number of order conflicts
    pub order_conflict_count: usize,
    /// Number of changes involved
    pub involved_change_count: usize,
}

impl ConflictSummary {
    /// Create a summary from a conflict tracker.
    pub fn from_tracker(tracker: &ConflictTracker) -> Self {
        Self {
            zombie_count: tracker.zombie_count(),
            missing_context_count: tracker.missing_context_count(),
            order_conflict_count: tracker.order_conflict_count(),
            involved_change_count: tracker.involved_change_count(),
        }
    }

    /// Check if there are any conflicts.
    pub fn has_conflicts(&self) -> bool {
        self.zombie_count > 0 || self.missing_context_count > 0 || self.order_conflict_count > 0
    }

    /// Get the total number of conflicts.
    pub fn total(&self) -> usize {
        self.zombie_count + self.missing_context_count + self.order_conflict_count
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // =========================================================================
    // Test Helpers
    // =========================================================================

    fn make_vertex(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode {
            change: NodeId::new(change),
            start: ChangePosition::new(start),
            end: ChangePosition::new(end),
        }
    }

    fn make_position(change: u64, pos: u64) -> Position<NodeId> {
        Position {
            change: NodeId::new(change),
            pos: ChangePosition::new(pos),
        }
    }

    // =========================================================================
    // Zombie Conflict Tests
    // =========================================================================

    #[test]
    fn test_zombie_conflict_new() {
        let node = make_vertex(42, 0, 10);
        let zombie = ZombieConflict::new(node);

        assert_eq!(zombie.node, node);
        assert!(zombie.deleted_by.is_empty());
        assert!(zombie.connected_by.is_empty());
        assert!(zombie.inode.is_none());
    }

    #[test]
    fn test_zombie_conflict_deleted_by() {
        let node = make_vertex(42, 0, 10);
        let change = NodeId::new(100);
        let zombie = ZombieConflict::deleted_by(node, change);

        assert_eq!(zombie.node, node);
        assert_eq!(zombie.deleted_by.len(), 1);
        assert_eq!(zombie.deleted_by[0], change);
    }

    #[test]
    fn test_zombie_conflict_add_changes() {
        let node = make_vertex(42, 0, 10);
        let mut zombie = ZombieConflict::new(node);

        zombie.add_deleted_by(NodeId::new(100));
        zombie.add_connected_by(NodeId::new(200));

        assert_eq!(zombie.deleted_by.len(), 1);
        assert_eq!(zombie.connected_by.len(), 1);
    }

    #[test]
    fn test_zombie_conflict_no_duplicates() {
        let node = make_vertex(42, 0, 10);
        let mut zombie = ZombieConflict::new(node);

        zombie.add_deleted_by(NodeId::new(100));
        zombie.add_deleted_by(NodeId::new(100)); // Duplicate

        assert_eq!(zombie.deleted_by.len(), 1);
    }

    #[test]
    fn test_zombie_conflict_with_inode() {
        let node = make_vertex(42, 0, 10);
        let inode = make_position(10, 0);
        let zombie = ZombieConflict::new(node).with_inode(inode);

        assert_eq!(zombie.inode, Some(inode));
    }

    #[test]
    fn test_zombie_conflict_is_resolved() {
        let node = make_vertex(42, 0, 10);
        let zombie = ZombieConflict::new(node);

        // No connections means resolved
        assert!(zombie.is_resolved());

        let mut zombie2 = ZombieConflict::new(node);
        zombie2.add_connected_by(NodeId::new(100));

        // Has connections means not resolved
        assert!(!zombie2.is_resolved());
    }

    // =========================================================================
    // Missing Context Tests
    // =========================================================================

    #[test]
    fn test_missing_context_new() {
        let pos = make_position(42, 100);
        let change = NodeId::new(50);
        let conflict = MissingContextConflict::new(pos, true, change);

        assert_eq!(conflict.position, pos);
        assert!(conflict.is_predecessor);
        assert_eq!(conflict.during_change, change);
        assert!(conflict.expected_change.is_none());
    }

    #[test]
    fn test_missing_context_up_context() {
        let pos = make_position(42, 100);
        let change = NodeId::new(50);
        let conflict = MissingContextConflict::predecessors(pos, change);

        assert!(conflict.is_predecessor);
    }

    #[test]
    fn test_missing_context_down_context() {
        let pos = make_position(42, 100);
        let change = NodeId::new(50);
        let conflict = MissingContextConflict::successors(pos, change);

        assert!(!conflict.is_predecessor);
    }

    #[test]
    fn test_missing_context_with_expected() {
        let pos = make_position(42, 100);
        let change = NodeId::new(50);
        let expected = Hash::of(b"expected change");
        let conflict = MissingContextConflict::new(pos, true, change).with_expected(expected);

        assert_eq!(conflict.expected_change, Some(expected));
    }

    // =========================================================================
    // Order Conflict Tests
    // =========================================================================

    #[test]
    fn test_order_conflict_new() {
        let pos = make_position(42, 100);
        let conflict = OrderConflict::new(pos);

        assert_eq!(conflict.position, pos);
        assert!(conflict.vertices.is_empty());
        assert!(conflict.changes.is_empty());
    }

    #[test]
    fn test_order_conflict_add_insertion() {
        let pos = make_position(42, 100);
        let mut conflict = OrderConflict::new(pos);

        let v1 = make_vertex(100, 0, 10);
        let v2 = make_vertex(200, 0, 20);

        conflict.add_insertion(v1, NodeId::new(100));
        conflict.add_insertion(v2, NodeId::new(200));

        assert_eq!(conflict.conflict_count(), 2);
        assert_eq!(conflict.changes.len(), 2);
    }

    #[test]
    fn test_order_conflict_no_duplicates() {
        let pos = make_position(42, 100);
        let mut conflict = OrderConflict::new(pos);

        let v1 = make_vertex(100, 0, 10);

        conflict.add_insertion(v1, NodeId::new(100));
        conflict.add_insertion(v1, NodeId::new(100)); // Duplicate

        assert_eq!(conflict.conflict_count(), 1);
    }

    // =========================================================================
    // Conflict Tracker Tests
    // =========================================================================

    #[test]
    fn test_conflict_tracker_new() {
        let tracker = ConflictTracker::new();

        assert!(!tracker.has_conflicts());
        assert!(tracker.is_empty());
        assert_eq!(tracker.total_conflict_count(), 0);
    }

    #[test]
    fn test_conflict_tracker_add_zombie() {
        let mut tracker = ConflictTracker::new();
        let node = make_vertex(42, 0, 10);
        let zombie = ZombieConflict::new(node);

        tracker.add_zombie(zombie);

        assert!(tracker.has_zombies());
        assert!(tracker.has_conflicts());
        assert_eq!(tracker.zombie_count(), 1);
    }

    #[test]
    fn test_conflict_tracker_add_zombie_vertex() {
        let mut tracker = ConflictTracker::new();
        let node = make_vertex(42, 0, 10);

        tracker.add_zombie_vertex(node);

        assert!(tracker.has_zombies());
        assert_eq!(tracker.zombie_count(), 1);
    }

    #[test]
    fn test_conflict_tracker_add_missing_context() {
        let mut tracker = ConflictTracker::new();
        let pos = make_position(42, 100);
        let conflict = MissingContextConflict::predecessors(pos, NodeId::new(50));

        tracker.add_missing_context(conflict);

        assert!(tracker.has_missing_contexts());
        assert!(tracker.has_conflicts());
        assert_eq!(tracker.missing_context_count(), 1);
    }

    #[test]
    fn test_conflict_tracker_add_order_conflict() {
        let mut tracker = ConflictTracker::new();
        let pos = make_position(42, 100);
        let conflict = OrderConflict::new(pos);

        tracker.add_order_conflict(conflict);

        assert!(tracker.has_order_conflicts());
        assert!(tracker.has_conflicts());
        assert_eq!(tracker.order_conflict_count(), 1);
    }

    #[test]
    fn test_conflict_tracker_total_count() {
        let mut tracker = ConflictTracker::new();

        tracker.add_zombie_vertex(make_vertex(42, 0, 10));
        tracker.add_missing_context(MissingContextConflict::predecessors(
            make_position(42, 100),
            NodeId::new(50),
        ));
        tracker.add_order_conflict(OrderConflict::new(make_position(42, 200)));

        assert_eq!(tracker.total_conflict_count(), 3);
    }

    #[test]
    fn test_conflict_tracker_involved_changes() {
        let mut tracker = ConflictTracker::new();

        let mut zombie = ZombieConflict::new(make_vertex(42, 0, 10));
        zombie.add_deleted_by(NodeId::new(100));
        zombie.add_connected_by(NodeId::new(200));
        tracker.add_zombie(zombie);

        assert!(tracker.involved_change_count() >= 2);
    }

    #[test]
    fn test_conflict_tracker_clear() {
        let mut tracker = ConflictTracker::new();

        tracker.add_zombie_vertex(make_vertex(42, 0, 10));
        tracker.add_missing_context(MissingContextConflict::predecessors(
            make_position(42, 100),
            NodeId::new(50),
        ));

        assert!(tracker.has_conflicts());

        tracker.clear();

        assert!(!tracker.has_conflicts());
        assert!(tracker.is_empty());
    }

    // =========================================================================
    // Conflict Summary Tests
    // =========================================================================

    #[test]
    fn test_conflict_summary_from_tracker() {
        let mut tracker = ConflictTracker::new();

        tracker.add_zombie_vertex(make_vertex(42, 0, 10));
        tracker.add_zombie_vertex(make_vertex(43, 0, 10));
        tracker.add_missing_context(MissingContextConflict::predecessors(
            make_position(42, 100),
            NodeId::new(50),
        ));

        let summary = ConflictSummary::from_tracker(&tracker);

        assert_eq!(summary.zombie_count, 2);
        assert_eq!(summary.missing_context_count, 1);
        assert_eq!(summary.order_conflict_count, 0);
        assert!(summary.has_conflicts());
        assert_eq!(summary.total(), 3);
    }

    #[test]
    fn test_conflict_summary_empty() {
        let tracker = ConflictTracker::new();
        let summary = ConflictSummary::from_tracker(&tracker);

        assert!(!summary.has_conflicts());
        assert_eq!(summary.total(), 0);
    }

    #[test]
    fn test_conflict_summary_default() {
        let summary = ConflictSummary::default();

        assert!(!summary.has_conflicts());
        assert_eq!(summary.zombie_count, 0);
        assert_eq!(summary.missing_context_count, 0);
        assert_eq!(summary.order_conflict_count, 0);
    }
}
