//! Workspace for change application
//!
//! This module provides the [`Workspace`] type, which holds temporary state
//! during the application of a change to the repository graph. The workspace
//! is designed to be reused across multiple change applications for efficiency.
//!
//! # Overview
//!
//! When applying a change, we need to track various pieces of information:
//!
//! - **Context tracking**: Where new vertices should be inserted
//! - **Edge operations**: Edges to add, delete, or modify
//! - **Conflict detection**: Zombies, missing contexts, cycles
//! - **Parent/child relationships**: For folder structure maintenance
//!
//! The `Workspace` provides a centralized place to manage this state.
//!
//! # Memory Efficiency
//!
//! The workspace is designed to be cleared and reused rather than recreated:
//!
//! ```rust,ignore
//! let mut workspace = Workspace::new();
//!
//! // Apply first change
//! apply_change(&mut txn, &mut stack, &change1, &mut workspace)?;
//! workspace.clear(); // Reuse the workspace
//!
//! // Apply second change
//! apply_change(&mut txn, &mut stack, &change2, &mut workspace)?;
//! ```
//!
//! # Conflict Tracking
//!
//! During application, various conflicts may be detected:
//!
//! - **Missing context**: A span referenced by the change doesn't exist
//! - **Zombie vertices**: Deleted content that's being resurrected
//! - **Cyclic paths**: Operations that would create cycles in the graph
//!
//! The workspace tracks these for later resolution or reporting.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::apply::Workspace;
//! use atomic_core::types::{Position, NodeId, ChangePosition, EdgeFlags};
//!
//! let mut workspace = Workspace::new();
//!
//! // Add context vertices
//! workspace.add_up_context(Position::ROOT);
//!
//! // Track edges to add
//! workspace.add_pending_edge(
//!     Position::ROOT,
//!     Position::new(NodeId::new(1), ChangePosition::new(0)),
//!     EdgeFlags::BLOCK,
//! );
//!
//! // Check workspace state
//! assert!(!workspace.is_empty());
//! assert_eq!(workspace.up_context_count(), 1);
//!
//! // Clear for reuse
//! workspace.clear();
//! assert!(workspace.is_empty());
//! ```

use std::collections::{HashMap, HashSet};

#[allow(unused_imports)]
use crate::types::{ChangePosition, EdgeFlags, GraphNode, Hash, Inode, NodeId, Position};

/// A pending edge operation to be applied to the graph.
///
/// This represents an edge that needs to be added as part of applying
/// a change. Edges connect vertices and define the structure of the graph.
///
/// # Fields
///
/// - `from`: Source position (where the edge starts)
/// - `to`: Destination position (where the edge ends)
/// - `flag`: Edge type flags (BLOCK, FOLDER, DELETED, etc.)
/// - `introduced_by`: The change that introduced this edge (if known)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PendingEdge {
    /// The source position of the edge.
    pub from: Position<NodeId>,
    /// The destination position of the edge.
    pub to: Position<NodeId>,
    /// The edge flags.
    pub flag: EdgeFlags,
    /// The change that introduced this edge (for tracking provenance).
    pub introduced_by: Option<NodeId>,
}

impl PendingEdge {
    /// Create a new pending edge.
    ///
    /// # Arguments
    ///
    /// * `from` - Source position
    /// * `to` - Destination position
    /// * `flag` - Edge flags
    pub fn new(from: Position<NodeId>, to: Position<NodeId>, flag: EdgeFlags) -> Self {
        Self {
            from,
            to,
            flag,
            introduced_by: None,
        }
    }

    /// Create a pending edge with an introduced_by reference.
    ///
    /// # Arguments
    ///
    /// * `from` - Source position
    /// * `to` - Destination position
    /// * `flag` - Edge flags
    /// * `introduced_by` - The change that introduced this edge
    pub fn with_introduced_by(
        from: Position<NodeId>,
        to: Position<NodeId>,
        flag: EdgeFlags,
        introduced_by: NodeId,
    ) -> Self {
        Self {
            from,
            to,
            flag,
            introduced_by: Some(introduced_by),
        }
    }

    /// Check if this is a deletion edge.
    pub fn is_deletion(&self) -> bool {
        self.flag.contains(EdgeFlags::DELETED)
    }

    /// Check if this is a folder edge.
    pub fn is_folder(&self) -> bool {
        self.flag.contains(EdgeFlags::FOLDER)
    }

    /// Check if this is a parent edge.
    pub fn is_parent(&self) -> bool {
        self.flag.contains(EdgeFlags::PARENT)
    }
}

/// Information about a missing context during application.
///
/// When a change references a position that doesn't exist in the graph,
/// this structure records the details for conflict resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MissingContext {
    /// The position that was expected but not found.
    pub position: Position<NodeId>,
    /// Whether this is an up-context (predecessor) or down-context (successor).
    pub is_predecessor: bool,
    /// The change being applied when this was detected.
    pub during_change: Option<NodeId>,
}

impl MissingContext {
    /// Create a new missing context record.
    pub fn new(position: Position<NodeId>, is_predecessor: bool) -> Self {
        Self {
            position,
            is_predecessor,
            during_change: None,
        }
    }

    /// Create a missing up-context record.
    pub fn predecessors(position: Position<NodeId>) -> Self {
        Self::new(position, true)
    }

    /// Create a missing down-context record.
    pub fn successors(position: Position<NodeId>) -> Self {
        Self::new(position, false)
    }

    /// Set the change during which this was detected.
    pub fn with_change(mut self, change: NodeId) -> Self {
        self.during_change = Some(change);
        self
    }
}

/// A zombie span that was resurrected during application.
///
/// Zombies occur when content that was deleted by one change is
/// referenced by another change. This creates a conflict that
/// needs to be handled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Zombie {
    /// The span that was found to be a zombie.
    pub node: GraphNode<NodeId>,
    /// The inode this zombie belongs to (if known).
    pub inode: Option<Inode>,
}

impl Zombie {
    /// Create a new zombie record.
    pub fn new(node: GraphNode<NodeId>) -> Self {
        Self { node, inode: None }
    }

    /// Create a zombie with an associated inode.
    pub fn with_inode(node: GraphNode<NodeId>, inode: Inode) -> Self {
        Self {
            node,
            inode: Some(inode),
        }
    }
}

/// Workspace for holding temporary state during change application.
///
/// The workspace accumulates state as a change is being applied:
/// - Context vertices for new insertions
/// - Pending edge operations
/// - Detected conflicts and issues
/// - Parent/child relationships
///
/// # Design Goals
///
/// 1. **Reusability**: Clear and reuse rather than reallocate
/// 2. **Efficiency**: Use appropriate data structures for each use case
/// 3. **Completeness**: Track all state needed for correct application
///
/// # Usage Pattern
///
/// ```rust
/// use atomic_core::apply::Workspace;
///
/// // Create once, reuse many times
/// let mut workspace = Workspace::new();
///
/// // For each change application:
/// // 1. Add context from the change
/// // 2. Process atoms
/// // 3. Check for conflicts
/// // 4. Clear for next use
/// workspace.clear();
/// ```
#[derive(Debug, Clone)]
pub struct Workspace {
    // =========================================================================
    // Context Tracking
    // =========================================================================
    /// Up-context positions (predecessors for new vertices).
    predecessors: Vec<Position<NodeId>>,

    /// Down-context positions (successors for new vertices).
    successors: Vec<Position<NodeId>>,

    // =========================================================================
    // Edge Operations
    // =========================================================================
    /// Pending edges to be added to the graph.
    pending_edges: Vec<PendingEdge>,

    /// Edges that have been marked for deletion.
    deleted_edges: HashSet<(Position<NodeId>, Position<NodeId>)>,

    // =========================================================================
    // Parent/Child Tracking
    // =========================================================================
    /// Map from child to parent positions (for folder structure).
    parents: HashMap<Position<NodeId>, Position<NodeId>>,

    /// Map from parent to children (for folder iteration).
    children: HashMap<Position<NodeId>, Vec<Position<NodeId>>>,

    // =========================================================================
    // Conflict Tracking
    // =========================================================================
    /// Missing context vertices detected during application.
    missing_contexts: Vec<MissingContext>,

    /// Zombie vertices that were resurrected.
    zombies: Vec<Zombie>,

    /// Positions that have been verified as rooted (connected to graph root).
    rooted: HashSet<Position<NodeId>>,

    // =========================================================================
    // Temporary Buffers
    // =========================================================================
    /// Buffer for adjacency iteration results.
    adjacency_buffer: Vec<Position<NodeId>>,

    /// Stack for graph traversal algorithms.
    traversal_stack: Vec<Position<NodeId>>,

    /// Set for visited tracking during traversal.
    visited: HashSet<Position<NodeId>>,
}

impl Workspace {
    /// Create a new empty workspace.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::apply::Workspace;
    ///
    /// let workspace = Workspace::new();
    /// assert!(workspace.is_empty());
    /// ```
    pub fn new() -> Self {
        Self {
            predecessors: Vec::new(),
            successors: Vec::new(),
            pending_edges: Vec::new(),
            deleted_edges: HashSet::new(),
            parents: HashMap::new(),
            children: HashMap::new(),
            missing_contexts: Vec::new(),
            zombies: Vec::new(),
            rooted: HashSet::new(),
            adjacency_buffer: Vec::new(),
            traversal_stack: Vec::new(),
            visited: HashSet::new(),
        }
    }

    /// Create a workspace with pre-allocated capacity.
    ///
    /// Use this when you know approximately how many items will be processed.
    ///
    /// # Arguments
    ///
    /// * `context_capacity` - Expected number of context positions
    /// * `edge_capacity` - Expected number of edges
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::apply::Workspace;
    ///
    /// // Pre-allocate for a large change
    /// let workspace = Workspace::with_capacity(100, 500);
    /// ```
    pub fn with_capacity(context_capacity: usize, edge_capacity: usize) -> Self {
        Self {
            predecessors: Vec::with_capacity(context_capacity),
            successors: Vec::with_capacity(context_capacity),
            pending_edges: Vec::with_capacity(edge_capacity),
            deleted_edges: HashSet::with_capacity(edge_capacity / 10),
            parents: HashMap::with_capacity(context_capacity),
            children: HashMap::with_capacity(context_capacity),
            missing_contexts: Vec::new(),
            zombies: Vec::new(),
            rooted: HashSet::with_capacity(context_capacity),
            adjacency_buffer: Vec::with_capacity(32),
            traversal_stack: Vec::with_capacity(64),
            visited: HashSet::with_capacity(context_capacity),
        }
    }

    /// Clear all workspace state for reuse.
    ///
    /// This clears all collections but retains their allocated capacity,
    /// making subsequent uses more efficient.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::apply::Workspace;
    /// use atomic_core::types::Position;
    ///
    /// let mut workspace = Workspace::new();
    /// workspace.add_up_context(Position::ROOT);
    /// assert!(!workspace.is_empty());
    ///
    /// workspace.clear();
    /// assert!(workspace.is_empty());
    /// ```
    pub fn clear(&mut self) {
        self.predecessors.clear();
        self.successors.clear();
        self.pending_edges.clear();
        self.deleted_edges.clear();
        self.parents.clear();
        self.children.clear();
        self.missing_contexts.clear();
        self.zombies.clear();
        self.rooted.clear();
        self.adjacency_buffer.clear();
        self.traversal_stack.clear();
        self.visited.clear();
    }

    /// Check if the workspace is empty (no state accumulated).
    ///
    /// # Returns
    ///
    /// `true` if all collections are empty.
    pub fn is_empty(&self) -> bool {
        self.predecessors.is_empty()
            && self.successors.is_empty()
            && self.pending_edges.is_empty()
            && self.deleted_edges.is_empty()
            && self.missing_contexts.is_empty()
            && self.zombies.is_empty()
    }

    // =========================================================================
    // Context Management
    // =========================================================================

    /// Add an up-context position (predecessor).
    ///
    /// Up-context vertices are the predecessors where new content will be
    /// inserted after.
    pub fn add_up_context(&mut self, pos: Position<NodeId>) {
        self.predecessors.push(pos);
    }

    /// Add a down-context position (successor).
    ///
    /// Down-context vertices are the successors where new content will be
    /// inserted before.
    pub fn add_down_context(&mut self, pos: Position<NodeId>) {
        self.successors.push(pos);
    }

    /// Get all up-context positions.
    pub fn predecessors(&self) -> &[Position<NodeId>] {
        &self.predecessors
    }

    /// Get all down-context positions.
    pub fn successors(&self) -> &[Position<NodeId>] {
        &self.successors
    }

    /// Get the number of up-context positions.
    pub fn up_context_count(&self) -> usize {
        self.predecessors.len()
    }

    /// Get the number of down-context positions.
    pub fn down_context_count(&self) -> usize {
        self.successors.len()
    }

    /// Clear context positions (up and down).
    pub fn clear_context(&mut self) {
        self.predecessors.clear();
        self.successors.clear();
    }

    // =========================================================================
    // Edge Management
    // =========================================================================

    /// Add a pending edge to be created.
    pub fn add_pending_edge(
        &mut self,
        from: Position<NodeId>,
        to: Position<NodeId>,
        flag: EdgeFlags,
    ) {
        self.pending_edges.push(PendingEdge::new(from, to, flag));
    }

    /// Add a pending edge with provenance tracking.
    pub fn add_pending_edge_with_provenance(
        &mut self,
        from: Position<NodeId>,
        to: Position<NodeId>,
        flag: EdgeFlags,
        introduced_by: NodeId,
    ) {
        self.pending_edges.push(PendingEdge::with_introduced_by(
            from,
            to,
            flag,
            introduced_by,
        ));
    }

    /// Mark an edge as deleted.
    pub fn mark_edge_deleted(&mut self, from: Position<NodeId>, to: Position<NodeId>) {
        self.deleted_edges.insert((from, to));
    }

    /// Check if an edge is marked as deleted.
    pub fn is_edge_deleted(&self, from: &Position<NodeId>, to: &Position<NodeId>) -> bool {
        self.deleted_edges.contains(&(*from, *to))
    }

    /// Get all pending edges.
    pub fn pending_edges(&self) -> &[PendingEdge] {
        &self.pending_edges
    }

    /// Get the number of pending edges.
    pub fn pending_edge_count(&self) -> usize {
        self.pending_edges.len()
    }

    /// Take all pending edges, leaving the vector empty.
    pub fn take_pending_edges(&mut self) -> Vec<PendingEdge> {
        std::mem::take(&mut self.pending_edges)
    }

    // =========================================================================
    // Parent/Child Tracking
    // =========================================================================

    /// Set the parent of a position.
    pub fn set_parent(&mut self, child: Position<NodeId>, parent: Position<NodeId>) {
        self.parents.insert(child, parent);
        self.children.entry(parent).or_default().push(child);
    }

    /// Get the parent of a position.
    pub fn get_parent(&self, child: &Position<NodeId>) -> Option<Position<NodeId>> {
        self.parents.get(child).copied()
    }

    /// Get the children of a position.
    pub fn get_children(&self, parent: &Position<NodeId>) -> Option<&[Position<NodeId>]> {
        self.children.get(parent).map(|v| v.as_slice())
    }

    // =========================================================================
    // Conflict Tracking
    // =========================================================================

    /// Record a missing context.
    pub fn add_missing_context(&mut self, ctx: MissingContext) {
        self.missing_contexts.push(ctx);
    }

    /// Record a missing up-context position.
    pub fn add_missing_up_context(&mut self, pos: Position<NodeId>) {
        self.missing_contexts
            .push(MissingContext::predecessors(pos));
    }

    /// Record a missing down-context position.
    pub fn add_missing_down_context(&mut self, pos: Position<NodeId>) {
        self.missing_contexts.push(MissingContext::successors(pos));
    }

    /// Get all missing contexts.
    pub fn missing_contexts(&self) -> &[MissingContext] {
        &self.missing_contexts
    }

    /// Check if any missing contexts were detected.
    pub fn has_missing_contexts(&self) -> bool {
        !self.missing_contexts.is_empty()
    }

    /// Record a zombie span.
    pub fn add_zombie(&mut self, zombie: Zombie) {
        self.zombies.push(zombie);
    }

    /// Record a zombie span by its span.
    pub fn add_zombie_vertex(&mut self, node: GraphNode<NodeId>) {
        self.zombies.push(Zombie::new(node));
    }

    /// Get all zombies.
    pub fn zombies(&self) -> &[Zombie] {
        &self.zombies
    }

    /// Check if any zombies were detected.
    pub fn has_zombies(&self) -> bool {
        !self.zombies.is_empty()
    }

    /// Check if any conflicts were detected (missing contexts or zombies).
    pub fn has_conflicts(&self) -> bool {
        self.has_missing_contexts() || self.has_zombies()
    }

    // =========================================================================
    // Rooted Tracking
    // =========================================================================

    /// Mark a position as verified rooted.
    pub fn mark_rooted(&mut self, pos: Position<NodeId>) {
        self.rooted.insert(pos);
    }

    /// Check if a position has been verified as rooted.
    pub fn is_rooted(&self, pos: &Position<NodeId>) -> bool {
        self.rooted.contains(pos)
    }

    // =========================================================================
    // Temporary Buffers
    // =========================================================================

    /// Get a mutable reference to the adjacency buffer.
    ///
    /// This buffer can be used for temporary storage during adjacency iteration.
    /// The caller should clear it before use.
    pub fn adjacency_buffer(&mut self) -> &mut Vec<Position<NodeId>> {
        &mut self.adjacency_buffer
    }

    /// Get a mutable reference to the traversal stack.
    ///
    /// This stack can be used for graph traversal algorithms.
    /// The caller should clear it before use.
    pub fn traversal_stack(&mut self) -> &mut Vec<Position<NodeId>> {
        &mut self.traversal_stack
    }

    /// Get a mutable reference to the visited set.
    ///
    /// This set can be used to track visited positions during traversal.
    /// The caller should clear it before use.
    pub fn visited(&mut self) -> &mut HashSet<Position<NodeId>> {
        &mut self.visited
    }

    // =========================================================================
    // Statistics
    // =========================================================================

    /// Get statistics about the workspace state.
    pub fn stats(&self) -> WorkspaceStats {
        WorkspaceStats {
            up_context_count: self.predecessors.len(),
            down_context_count: self.successors.len(),
            pending_edge_count: self.pending_edges.len(),
            deleted_edge_count: self.deleted_edges.len(),
            parent_count: self.parents.len(),
            missing_context_count: self.missing_contexts.len(),
            zombie_count: self.zombies.len(),
            rooted_count: self.rooted.len(),
        }
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the workspace state.
///
/// Useful for debugging and performance monitoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceStats {
    /// Number of up-context positions.
    pub up_context_count: usize,
    /// Number of down-context positions.
    pub down_context_count: usize,
    /// Number of pending edges.
    pub pending_edge_count: usize,
    /// Number of deleted edges.
    pub deleted_edge_count: usize,
    /// Number of parent relationships.
    pub parent_count: usize,
    /// Number of missing contexts.
    pub missing_context_count: usize,
    /// Number of zombies.
    pub zombie_count: usize,
    /// Number of rooted positions.
    pub rooted_count: usize,
}

impl WorkspaceStats {
    /// Check if all counts are zero.
    pub fn is_empty(&self) -> bool {
        self.up_context_count == 0
            && self.down_context_count == 0
            && self.pending_edge_count == 0
            && self.deleted_edge_count == 0
            && self.missing_context_count == 0
            && self.zombie_count == 0
    }

    /// Get the total number of items tracked.
    pub fn total(&self) -> usize {
        self.up_context_count
            + self.down_context_count
            + self.pending_edge_count
            + self.deleted_edge_count
            + self.parent_count
            + self.missing_context_count
            + self.zombie_count
            + self.rooted_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // PendingEdge Tests
    // =========================================================================

    #[test]
    fn test_pending_edge_new() {
        let from = Position::ROOT;
        let to = Position::new(NodeId::new(1), ChangePosition::new(0));
        let flag = EdgeFlags::BLOCK;

        let edge = PendingEdge::new(from, to, flag);

        assert_eq!(edge.from, from);
        assert_eq!(edge.to, to);
        assert_eq!(edge.flag, flag);
        assert!(edge.introduced_by.is_none());
    }

    #[test]
    fn test_pending_edge_with_introduced_by() {
        let from = Position::ROOT;
        let to = Position::new(NodeId::new(1), ChangePosition::new(0));
        let flag = EdgeFlags::BLOCK;
        let introduced_by = NodeId::new(5);

        let edge = PendingEdge::with_introduced_by(from, to, flag, introduced_by);

        assert_eq!(edge.introduced_by, Some(introduced_by));
    }

    #[test]
    fn test_pending_edge_is_deletion() {
        let edge = PendingEdge::new(
            Position::ROOT,
            Position::ROOT,
            EdgeFlags::BLOCK | EdgeFlags::DELETED,
        );
        assert!(edge.is_deletion());

        let edge = PendingEdge::new(Position::ROOT, Position::ROOT, EdgeFlags::BLOCK);
        assert!(!edge.is_deletion());
    }

    #[test]
    fn test_pending_edge_is_folder() {
        let edge = PendingEdge::new(Position::ROOT, Position::ROOT, EdgeFlags::FOLDER);
        assert!(edge.is_folder());

        let edge = PendingEdge::new(Position::ROOT, Position::ROOT, EdgeFlags::BLOCK);
        assert!(!edge.is_folder());
    }

    #[test]
    fn test_pending_edge_is_parent() {
        let edge = PendingEdge::new(Position::ROOT, Position::ROOT, EdgeFlags::PARENT);
        assert!(edge.is_parent());

        let edge = PendingEdge::new(Position::ROOT, Position::ROOT, EdgeFlags::BLOCK);
        assert!(!edge.is_parent());
    }

    // =========================================================================
    // MissingContext Tests
    // =========================================================================

    #[test]
    fn test_missing_context_up() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let ctx = MissingContext::predecessors(pos);

        assert_eq!(ctx.position, pos);
        assert!(ctx.is_predecessor);
        assert!(ctx.during_change.is_none());
    }

    #[test]
    fn test_missing_context_down() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let ctx = MissingContext::successors(pos);

        assert_eq!(ctx.position, pos);
        assert!(!ctx.is_predecessor);
    }

    #[test]
    fn test_missing_context_with_change() {
        let pos = Position::ROOT;
        let change = NodeId::new(10);
        let ctx = MissingContext::predecessors(pos).with_change(change);

        assert_eq!(ctx.during_change, Some(change));
    }

    // =========================================================================
    // Zombie Tests
    // =========================================================================

    #[test]
    fn test_zombie_new() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let zombie = Zombie::new(node);

        assert_eq!(zombie.node, node);
        assert!(zombie.inode.is_none());
    }

    #[test]
    fn test_zombie_with_inode() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let inode = Inode::new(42);
        let zombie = Zombie::with_inode(node, inode);

        assert_eq!(zombie.node, node);
        assert_eq!(zombie.inode, Some(inode));
    }

    // =========================================================================
    // Workspace Basic Tests
    // =========================================================================

    #[test]
    fn test_workspace_new() {
        let workspace = Workspace::new();
        assert!(workspace.is_empty());
    }

    #[test]
    fn test_workspace_with_capacity() {
        let workspace = Workspace::with_capacity(100, 500);
        assert!(workspace.is_empty());
    }

    #[test]
    fn test_workspace_default() {
        let workspace = Workspace::default();
        assert!(workspace.is_empty());
    }

    #[test]
    fn test_workspace_clear() {
        let mut workspace = Workspace::new();

        // Add some state
        workspace.add_up_context(Position::ROOT);
        workspace.add_pending_edge(Position::ROOT, Position::ROOT, EdgeFlags::BLOCK);
        workspace.add_missing_up_context(Position::ROOT);

        assert!(!workspace.is_empty());

        workspace.clear();
        assert!(workspace.is_empty());
    }

    // =========================================================================
    // Context Tests
    // =========================================================================

    #[test]
    fn test_workspace_up_context() {
        let mut workspace = Workspace::new();
        let pos1 = Position::ROOT;
        let pos2 = Position::new(NodeId::new(1), ChangePosition::new(0));

        workspace.add_up_context(pos1);
        workspace.add_up_context(pos2);

        assert_eq!(workspace.up_context_count(), 2);
        assert_eq!(workspace.predecessors(), &[pos1, pos2]);
    }

    #[test]
    fn test_workspace_down_context() {
        let mut workspace = Workspace::new();
        let pos = Position::new(NodeId::new(2), ChangePosition::new(50));

        workspace.add_down_context(pos);

        assert_eq!(workspace.down_context_count(), 1);
        assert_eq!(workspace.successors(), &[pos]);
    }

    #[test]
    fn test_workspace_clear_context() {
        let mut workspace = Workspace::new();

        workspace.add_up_context(Position::ROOT);
        workspace.add_down_context(Position::ROOT);

        workspace.clear_context();

        assert_eq!(workspace.up_context_count(), 0);
        assert_eq!(workspace.down_context_count(), 0);
    }

    // =========================================================================
    // Edge Tests
    // =========================================================================

    #[test]
    fn test_workspace_pending_edges() {
        let mut workspace = Workspace::new();
        let from = Position::ROOT;
        let to = Position::new(NodeId::new(1), ChangePosition::new(0));

        workspace.add_pending_edge(from, to, EdgeFlags::BLOCK);

        assert_eq!(workspace.pending_edge_count(), 1);
        assert_eq!(workspace.pending_edges()[0].from, from);
        assert_eq!(workspace.pending_edges()[0].to, to);
    }

    #[test]
    fn test_workspace_pending_edges_with_provenance() {
        let mut workspace = Workspace::new();
        let from = Position::ROOT;
        let to = Position::ROOT;
        let introduced_by = NodeId::new(5);

        workspace.add_pending_edge_with_provenance(from, to, EdgeFlags::BLOCK, introduced_by);

        assert_eq!(
            workspace.pending_edges()[0].introduced_by,
            Some(introduced_by)
        );
    }

    #[test]
    fn test_workspace_take_pending_edges() {
        let mut workspace = Workspace::new();
        workspace.add_pending_edge(Position::ROOT, Position::ROOT, EdgeFlags::BLOCK);
        workspace.add_pending_edge(Position::ROOT, Position::ROOT, EdgeFlags::FOLDER);

        let edges = workspace.take_pending_edges();

        assert_eq!(edges.len(), 2);
        assert_eq!(workspace.pending_edge_count(), 0);
    }

    #[test]
    fn test_workspace_deleted_edges() {
        let mut workspace = Workspace::new();
        let from = Position::ROOT;
        let to = Position::new(NodeId::new(1), ChangePosition::new(0));

        assert!(!workspace.is_edge_deleted(&from, &to));

        workspace.mark_edge_deleted(from, to);

        assert!(workspace.is_edge_deleted(&from, &to));
    }

    // =========================================================================
    // Parent/Child Tests
    // =========================================================================

    #[test]
    fn test_workspace_parent_child() {
        let mut workspace = Workspace::new();
        let parent = Position::ROOT;
        let child1 = Position::new(NodeId::new(1), ChangePosition::new(0));
        let child2 = Position::new(NodeId::new(2), ChangePosition::new(0));

        workspace.set_parent(child1, parent);
        workspace.set_parent(child2, parent);

        assert_eq!(workspace.get_parent(&child1), Some(parent));
        assert_eq!(workspace.get_parent(&child2), Some(parent));

        let children = workspace.get_children(&parent).unwrap();
        assert_eq!(children.len(), 2);
        assert!(children.contains(&child1));
        assert!(children.contains(&child2));
    }

    #[test]
    fn test_workspace_no_parent() {
        let workspace = Workspace::new();
        let pos = Position::ROOT;

        assert!(workspace.get_parent(&pos).is_none());
        assert!(workspace.get_children(&pos).is_none());
    }

    // =========================================================================
    // Conflict Tracking Tests
    // =========================================================================

    #[test]
    fn test_workspace_missing_contexts() {
        let mut workspace = Workspace::new();
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));

        assert!(!workspace.has_missing_contexts());

        workspace.add_missing_up_context(pos);

        assert!(workspace.has_missing_contexts());
        assert_eq!(workspace.missing_contexts().len(), 1);
        assert!(workspace.missing_contexts()[0].is_predecessor);
    }

    #[test]
    fn test_workspace_zombies() {
        let mut workspace = Workspace::new();
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );

        assert!(!workspace.has_zombies());

        workspace.add_zombie_vertex(node);

        assert!(workspace.has_zombies());
        assert_eq!(workspace.zombies().len(), 1);
    }

    #[test]
    fn test_workspace_has_conflicts() {
        let mut workspace = Workspace::new();

        assert!(!workspace.has_conflicts());

        workspace.add_missing_up_context(Position::ROOT);
        assert!(workspace.has_conflicts());

        workspace.clear();
        assert!(!workspace.has_conflicts());

        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        workspace.add_zombie_vertex(node);
        assert!(workspace.has_conflicts());
    }

    // =========================================================================
    // Rooted Tracking Tests
    // =========================================================================

    #[test]
    fn test_workspace_rooted() {
        let mut workspace = Workspace::new();
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));

        assert!(!workspace.is_rooted(&pos));

        workspace.mark_rooted(pos);

        assert!(workspace.is_rooted(&pos));
    }

    // =========================================================================
    // Temporary Buffer Tests
    // =========================================================================

    #[test]
    fn test_workspace_buffers() {
        let mut workspace = Workspace::new();

        // Adjacency buffer
        workspace.adjacency_buffer().push(Position::ROOT);
        assert_eq!(workspace.adjacency_buffer().len(), 1);

        // Traversal stack
        workspace.traversal_stack().push(Position::ROOT);
        assert_eq!(workspace.traversal_stack().len(), 1);

        // Visited set
        workspace.visited().insert(Position::ROOT);
        assert!(workspace.visited().contains(&Position::ROOT));
    }

    // =========================================================================
    // Statistics Tests
    // =========================================================================

    #[test]
    fn test_workspace_stats_empty() {
        let workspace = Workspace::new();
        let stats = workspace.stats();

        assert!(stats.is_empty());
        assert_eq!(stats.total(), 0);
    }

    #[test]
    fn test_workspace_stats_populated() {
        let mut workspace = Workspace::new();

        workspace.add_up_context(Position::ROOT);
        workspace.add_down_context(Position::ROOT);
        workspace.add_pending_edge(Position::ROOT, Position::ROOT, EdgeFlags::BLOCK);
        workspace.mark_edge_deleted(Position::ROOT, Position::ROOT);
        workspace.add_missing_up_context(Position::ROOT);
        workspace.add_zombie_vertex(GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        ));
        workspace.mark_rooted(Position::ROOT);

        let stats = workspace.stats();

        assert_eq!(stats.up_context_count, 1);
        assert_eq!(stats.down_context_count, 1);
        assert_eq!(stats.pending_edge_count, 1);
        assert_eq!(stats.deleted_edge_count, 1);
        assert_eq!(stats.missing_context_count, 1);
        assert_eq!(stats.zombie_count, 1);
        assert_eq!(stats.rooted_count, 1);

        assert!(!stats.is_empty());
        assert!(stats.total() > 0);
    }

    #[test]
    fn test_workspace_stats_debug() {
        let stats = WorkspaceStats {
            up_context_count: 1,
            down_context_count: 2,
            pending_edge_count: 3,
            deleted_edge_count: 4,
            parent_count: 5,
            missing_context_count: 6,
            zombie_count: 7,
            rooted_count: 8,
        };

        let debug = format!("{:?}", stats);
        assert!(debug.contains("WorkspaceStats"));
    }
}
