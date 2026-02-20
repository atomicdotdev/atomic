//! Database trait abstractions for pristine storage
//!
//! This module defines the trait interfaces for interacting with the pristine
//! database. These traits provide a clean abstraction layer that:
//!
//! - Separates interface from implementation
//! - Enables testing with mock implementations
//! - Documents the expected behavior of database operations
//! - Allows for future alternative backends
//!
//! # Trait Hierarchy
//!
//! The traits form a hierarchy based on capability:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                         MutTxnT                                 │
//! │  (Full read-write access - commit, abort, modifications)       │
//! └─────────────────────────────────────────────────────────────────┘
//!                               │
//!                    extends all of:
//!                               │
//!          ┌────────────────────┼────────────────────┐
//!          ▼                    ▼                    ▼
//! ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
//! │   StackTxnT     │  │    TreeTxnT     │  │   GraphTxnT     │
//! │ (Stack/view     │  │ (File tree      │  │ (Base trait:    │
//! │  operations)    │  │  operations)    │  │  graph queries) │
//! └─────────────────┘  └─────────────────┘  └─────────────────┘
//!          │                    │                    │
//!          └────────────────────┼────────────────────┘
//!                               │
//!                    all extend:
//!                               │
//!                               ▼
//!                      ┌─────────────────┐
//!                      │   GraphTxnT     │
//!                      │ (Base trait)    │
//!                      └─────────────────┘
//! ```
//!
//! # Stacks: A Different Mental Model
//!
//! Unlike Git branches which fork history, Atomic **Stacks** are views of the
//! same underlying graph. Think of them like database views or saved queries:
//!
//! | Property | Git Branch | Atomic Stack |
//! |----------|------------|--------------|
//! | Data model | Pointer to commit | Sequence of applied changes |
//! | State tracking | HEAD commit hash | Merkle hash of change sequence |
//! | Switching | Checkout (changes files) | Just changes view context |
//! | "Merging" | 3-way merge of histories | Apply missing changes |
//! | Storage cost | Full history per branch | Shared graph, only metadata differs |
//!
//! When you create a new stack, you're not forking data—you're creating a new
//! perspective on the same repository graph.
//!
//! # Usage Example
//!
//! ```ignore
//! use atomic_core::pristine::{GraphTxnT, StackTxnT, TreeTxnT, MutTxnT};
//!
//! // Reading from the database (any transaction type)
//! fn count_files<T: TreeTxnT>(txn: &T) -> PristineResult<usize> {
//!     let mut count = 0;
//!     for result in txn.iter_tree()? {
//!         let _ = result?;
//!         count += 1;
//!     }
//!     Ok(count)
//! }
//!
//! // Writing to the database (requires MutTxnT)
//! fn create_stack<T: MutTxnT>(txn: &mut T, name: &str) -> PristineResult<StackState> {
//!     txn.open_or_create_stack(name)
//! }
//! ```
//!
//! # Implementation Notes
//!
//! All iterator-returning methods return `Box<dyn Iterator>` to avoid complex
//! lifetime issues with redb's borrowing model. This has a small performance
//! cost but greatly simplifies the API.

use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Merkle, NodeId, Position,
    SerializedGraphEdge,
};

use super::error::PristineError;

// GraphTxnT - Base Graph Operations

/// Read-only graph operations
///
/// This is the base trait that provides read access to the repository graph.
/// All other transaction traits extend this one.
///
/// # Graph Structure
///
/// The graph consists of:
/// - **Vertices**: Ranges of content within changes, identified by (change_id, start, end)
/// - **Edges**: Connections between vertices with flags indicating relationship type
///
/// # ID System
///
/// Atomic uses two ID systems:
/// - **External (Hash)**: Content-addressed, globally unique, used for sync
/// - **Internal (NodeId)**: Repository-local, compact, used for storage
///
/// This trait provides methods to translate between these two systems.
///
/// # Example
///
/// ```ignore
/// fn lookup_change<T: GraphTxnT>(txn: &T, hash: &Hash) -> PristineResult<Option<NodeId>> {
///     // Convert external hash to internal ID
///     let node_id = txn.get_internal(hash)?;
///
///     if let Some(id) = node_id {
///         // Verify we can convert back
///         let hash_back = txn.get_external(id)?;
///         assert_eq!(hash_back.as_ref(), Some(hash));
///     }
///
///     Ok(node_id)
/// }
/// ```
pub trait GraphTxnT {
    /// Iterator type for adjacency lists
    ///
    /// This returns edges from a span. The iterator yields `Result` to handle
    /// potential storage errors during iteration.
    type Adj: Iterator<Item = Result<SerializedGraphEdge, PristineError>>;

    /// Get the external hash for an internal node ID
    ///
    /// Translates a repository-local NodeId to the globally-unique content hash.
    ///
    /// # Arguments
    ///
    /// * `id` - The internal node identifier
    ///
    /// # Returns
    ///
    /// * `Ok(Some(hash))` - The corresponding hash
    /// * `Ok(None)` - The NodeId is not registered
    /// * `Err(_)` - Database error
    ///
    /// # Example
    ///
    /// ```ignore
    /// let hash = txn.get_external(node_id)?;
    /// if let Some(h) = hash {
    ///     println!("Change hash: {}", h);
    /// }
    /// ```
    fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError>;

    /// Get the internal node ID for an external hash
    ///
    /// Translates a globally-unique content hash to a repository-local NodeId.
    /// This is the inverse of `get_external`.
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash to look up
    ///
    /// # Returns
    ///
    /// * `Ok(Some(id))` - The corresponding internal ID
    /// * `Ok(None)` - The hash is not registered in this repository
    /// * `Err(_)` - Database error
    fn get_internal(&self, hash: &Hash) -> Result<Option<NodeId>, PristineError>;

    /// Initialize an adjacency iterator for a span
    ///
    /// Returns an iterator over edges from the given span that have flags
    /// within the specified range. This allows filtering edges by type.
    ///
    /// # Arguments
    ///
    /// * `span` - The source span
    /// * `min_flag` - Minimum edge flags (inclusive)
    /// * `max_flag` - Maximum edge flags (inclusive)
    ///
    /// # Returns
    ///
    /// An iterator yielding edges that match the flag criteria.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Get all non-deleted block edges
    /// let edges = txn.iter_adjacent(
    ///     span,
    ///     EdgeFlags::BLOCK,
    ///     EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
    /// )?;
    ///
    /// for result in edges {
    ///     let edge = result?;
    ///     println!("Edge to {:?}", edge.dest());
    /// }
    /// ```
    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<Self::Adj, PristineError>;

    /// Find the span containing a given position
    ///
    /// Given a position (change_id, byte_offset), finds the span that contains
    /// that byte. This is used when navigating the graph, as edges point to
    /// positions, not vertices.
    ///
    /// # Arguments
    ///
    /// * `pos` - The position to search for
    ///
    /// # Returns
    ///
    /// * `Ok(span)` - The span containing the position
    /// * `Err(BlockNotFound)` - No span contains this position
    ///
    /// # Example
    ///
    /// ```ignore
    /// let pos = edge.dest();
    /// let dest_vertex = txn.find_block(pos)?;
    /// // Now we can get edges from dest_vertex
    /// ```
    fn find_block(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError>;

    /// Find a block that ends at or after the given position.
    ///
    /// This is used for predecessors resolution where we need to find the span
    /// that ENDS at a position, not one that contains it. This is important
    /// when creating edges from an existing span to a new one.
    ///
    /// # Arguments
    ///
    /// * `pos` - The position to find (typically the end of a context span)
    ///
    /// # Returns
    ///
    /// The span that ends at or after the given position, or an error if not found.
    ///
    /// # Special Cases
    ///
    /// - ROOT position returns GraphNode::ROOT
    /// - Empty vertices (start == end == pos) are matched exactly
    /// - For non-empty vertices, finds one where end == pos (span ends at position)
    ///
    /// # Example
    ///
    /// ```ignore
    /// // For predecessors resolution, we want the span ending at this position
    /// let up_vertex = txn.find_block_end(up_pos)?;
    /// ```
    fn find_block_end(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError>;

    /// Check if a span exists in the graph
    ///
    /// Returns true if the span has at least one edge (vertices are
    /// implicitly defined by their edges).
    ///
    /// # Arguments
    ///
    /// * `span` - The span to check
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The span has edges
    /// * `Ok(false)` - The span has no edges
    /// * `Err(_)` - Database error
    fn has_vertex(&self, node: GraphNode<NodeId>) -> Result<bool, PristineError>;

    /// Get all edges from a span (convenience method)
    ///
    /// This is equivalent to `iter_adjacent` with full flag range, collecting
    /// results into a Vec.
    ///
    /// # Arguments
    ///
    /// * `span` - The source span
    ///
    /// # Returns
    ///
    /// A vector of all edges from this span.
    fn get_edges(
        &self,
        node: GraphNode<NodeId>,
    ) -> Result<Vec<SerializedGraphEdge>, PristineError> {
        let iter = self.iter_adjacent(node, EdgeFlags::empty(), EdgeFlags::all())?;
        iter.collect()
    }

    /// Get the type of a node (Change, Tag, or Attestation).
    ///
    /// This is a read-only operation available on both read and write
    /// transactions.
    ///
    /// # Returns
    ///
    /// * `Ok(Some(node_type::CHANGE))` - The node is a change
    /// * `Ok(Some(node_type::TAG))` - The node is a tag
    /// * `Ok(Some(node_type::ATTESTATION))` - The node is an attestation
    /// * `Ok(None)` - The node ID is not registered
    /// * `Err(_)` - Database error
    fn get_node_type(&self, node_id: NodeId) -> Result<Option<u8>, PristineError>;

    /// Get all nodes that depend on the given node (reverse dependency lookup).
    ///
    /// Returns a list of NodeIds that have registered a dependency on
    /// the given node. Used to find attestations that cover a change
    /// (filter results by `node_type::ATTESTATION`).
    fn get_rev_deps(&self, dep_id: NodeId) -> Result<Vec<NodeId>, PristineError>;
}

// StackState - Stack Metadata

/// Stack state information
///
/// A Stack represents a **view** of the repository graph. Unlike Git branches
/// which point to a commit and represent a fork of history, a Stack is an
/// ordered sequence of changes applied to the same shared graph.
///
/// # Key Properties
///
/// - **id**: Repository-local identifier for the stack
/// - **name**: Human-readable name (like "main", "feature-x")
/// - **state**: Merkle hash representing the cumulative state
/// - **change_count**: Number of changes applied to this stack
///
/// # Merkle State
///
/// The `state` field is a Merkle hash computed incrementally:
///
/// ```text
/// state_0 = Hash(empty)
/// state_n = Hash(state_{n-1} || change_hash_n)
/// ```
///
/// This allows efficient comparison of stack states:
/// - Same state → stacks have identical changes in identical order
/// - Different state → stacks differ somehow
///
/// # Example
///
/// ```
/// use atomic_core::pristine::StackState;
/// use atomic_core::types::Merkle;
///
/// let stack = StackState::new(1, "feature-login".to_string());
/// assert_eq!(stack.name, "feature-login");
/// assert_eq!(stack.change_count, 0);
/// assert_eq!(stack.state, Merkle::ZERO);
/// ```
/// Controls the lifecycle and edge-storage strategy for a stack.
///
/// # Two-Tier Graph Model
///
/// Atomic uses a two-tier graph model where edges live in different storage
/// locations depending on the stack kind:
///
/// - **Shared** stacks (dev, release, main) write edges to the global `GRAPH`
///   table. These edges are visible to all stacks and persist permanently.
/// - **Local** stacks (feature, bug, experiment) write edges to the
///   per-stack `STACK_GRAPH` table. These edges are only visible through the
///   overlay chain and are cascade-deleted when the stack is deleted.
///
/// # Overlay Chain
///
/// An local workspace's effective view is the union of its own `STACK_GRAPH`,
/// its parent's effective view (recursively), down to the global `GRAPH`:
///
/// ```text
/// feature-login view = STACK_GRAPH[feature-login]
///                     ∪ STACK_GRAPH[service-auth]   (parent)
///                     ∪ GRAPH                        (dev is Shared → stop)
/// ```
///
/// # Example
///
/// ```
/// use atomic_core::pristine::StackKind;
///
/// let kind = StackKind::Local;
/// assert_eq!(kind as u8, 0);
/// assert!(!kind.is_shared());
/// assert!(kind.is_local());
///
/// let kind = StackKind::Shared;
/// assert_eq!(kind as u8, 1);
/// assert!(kind.is_shared());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum StackKind {
    /// Ephemeral staging area (feature, bug, experiment).
    ///
    /// Edges are stored in `STACK_GRAPH[(stack_id, vertex)]`.
    /// Can be deleted cleanly at any time via cascade.
    Local = 0,

    /// Permanent promoted history (dev, release, main).
    ///
    /// Edges are stored in the global `GRAPH[vertex]`.
    /// Deletion is restricted; these stacks are the canonical record.
    Shared = 1,
}

impl StackKind {
    /// Check if this is a shared stack.
    #[inline]
    pub fn is_shared(self) -> bool {
        self == Self::Shared
    }

    /// Check if this is an local workspace.
    #[inline]
    pub fn is_local(self) -> bool {
        self == Self::Local
    }

    /// Convert from a raw u8 value.
    ///
    /// Returns `None` if the value is not a valid `StackKind`.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Local),
            1 => Some(Self::Shared),
            _ => None,
        }
    }
}

impl Default for StackKind {
    fn default() -> Self {
        Self::Shared
    }
}

impl std::fmt::Display for StackKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::Shared => write!(f, "shared"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackState {
    /// Stack ID (internal, repository-local)
    ///
    /// This is an auto-incrementing identifier assigned when the stack is created.
    /// It's used as part of the key in various tables.
    pub id: u64,

    /// Stack name (human-readable)
    ///
    /// This is the user-facing name like "main", "develop", or "feature-x".
    /// Stack names must be unique within a repository.
    pub name: String,

    /// Current Merkle state (cumulative hash of applied changes)
    ///
    /// This hash uniquely identifies the state of the stack. Two stacks with
    /// the same Merkle state have the exact same changes in the exact same order.
    pub state: Merkle,

    /// Number of changes applied to this stack
    ///
    /// This is the sequence number of the next change to be applied.
    /// If change_count is 5, changes 0-4 have been applied.
    pub change_count: u64,

    /// Stack kind (Local or Shared)
    ///
    /// Controls where edges are stored when changes are applied:
    /// - `Shared`: edges go to the global `GRAPH` table (permanent)
    /// - `Local`: edges go to `STACK_GRAPH[(stack_id, vertex)]` (ephemeral)
    pub kind: StackKind,

    /// Parent stack ID
    ///
    /// The stack this one was branched from. Used to build the overlay chain
    /// for graph traversal. Every stack except the root has a parent.
    ///
    /// - `None`: This is the root stack (e.g., "main"). Only one stack should
    ///   have `parent = None` — the root of the hierarchy.
    /// - `Some(id)`: The parent stack's internal ID. The parent can be either
    ///   Shared or Local. For example, `feature-login` might have
    ///   `parent = Some(service_auth_id)` which itself has
    ///   `parent = Some(dev_id)`.
    pub parent: Option<u64>,
}

impl Default for StackState {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            state: Merkle::ZERO,
            change_count: 0,
            kind: StackKind::Shared,
            parent: None,
        }
    }
}

impl StackState {
    /// Create a new shared stack state with the given name and no parent.
    ///
    /// This is the default constructor for backward compatibility. New code
    /// should prefer [`StackState::with_kind`] for explicit kind/parent.
    ///
    /// # Arguments
    ///
    /// * `id` - The internal stack identifier
    /// * `name` - The human-readable stack name
    ///
    /// # Example
    ///
    /// ```
    /// use atomic_core::pristine::StackState;
    ///
    /// let stack = StackState::new(1, "main".to_string());
    /// assert_eq!(stack.id, 1);
    /// assert_eq!(stack.name, "main");
    /// assert_eq!(stack.change_count, 0);
    /// assert!(stack.kind.is_shared());
    /// assert!(stack.parent.is_none());
    /// ```
    pub fn new(id: u64, name: String) -> Self {
        Self {
            id,
            name,
            state: Merkle::ZERO,
            change_count: 0,
            kind: StackKind::Shared,
            parent: None,
        }
    }

    /// Create a new stack with explicit kind and parent.
    ///
    /// # Arguments
    ///
    /// * `id` - The internal stack identifier
    /// * `name` - The human-readable stack name
    /// * `kind` - Whether this stack is Local or Shared
    /// * `parent` - The parent stack's ID (`None` for the root stack)
    ///
    /// # Example
    ///
    /// ```
    /// use atomic_core::pristine::{StackState, StackKind};
    ///
    /// // Create a shared "dev" stack parented on "main" (id=1)
    /// let dev = StackState::with_kind(2, "dev".to_string(), StackKind::Shared, Some(1));
    /// assert!(dev.kind.is_shared());
    /// assert_eq!(dev.parent, Some(1));
    ///
    /// // Create a local "feature" stack parented on "dev" (id=2)
    /// let feature = StackState::with_kind(3, "feature".to_string(), StackKind::Local, Some(2));
    /// assert!(feature.kind.is_local());
    /// assert_eq!(feature.parent, Some(2));
    /// ```
    pub fn with_kind(id: u64, name: String, kind: StackKind, parent: Option<u64>) -> Self {
        Self {
            id,
            name,
            state: Merkle::ZERO,
            change_count: 0,
            kind,
            parent,
        }
    }

    /// Check if the stack has any changes
    ///
    /// # Returns
    ///
    /// `true` if the stack has no changes applied.
    pub fn is_empty(&self) -> bool {
        self.change_count == 0
    }

    /// Check if this is the root stack (no parent).
    #[inline]
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }
}

// StackTxnT - Stack Operations

/// Stack operations
///
/// This trait provides read access to stack metadata and change logs.
/// Stacks are views of the graph that track which changes have been applied
/// and in what order.
///
/// # Stack vs Branch Conceptual Model
///
/// Think of a Stack like a playlist of songs (changes) from a shared music
/// library (the graph). Different playlists can contain different songs in
/// different orders, but they all reference the same library. "Merging"
/// playlists means adding songs from one playlist that the other doesn't have.
///
/// # Example
///
/// ```ignore
/// fn print_stack_info<T: StackTxnT>(txn: &T, name: &str) -> PristineResult<()> {
///     if let Some(stack) = txn.get_stack(name)? {
///         println!("Stack: {}", stack.name);
///         println!("Changes: {}", stack.change_count);
///         println!("State: {}", stack.state);
///
///         // List recent changes
///         for result in txn.iter_changes(&stack, 0)? {
///             let (seq, change_id, merkle) = result?;
///             let hash = txn.get_external(change_id)?.unwrap();
///             println!("  #{}: {}", seq, hash);
///         }
///     }
///     Ok(())
/// }
/// ```
pub trait StackTxnT: GraphTxnT {
    /// Look up a stack by its internal ID.
    ///
    /// This is used to resolve parent references when walking the overlay
    /// chain. Unlike [`get_stack`] which looks up by name, this looks up
    /// by the internal numeric ID stored in `StackState::id`.
    ///
    /// # Arguments
    ///
    /// * `id` - The internal stack identifier
    ///
    /// # Returns
    ///
    /// * `Ok(Some(state))` - The stack with this ID
    /// * `Ok(None)` - No stack with this ID
    /// * `Err(_)` - Database error
    fn get_stack_by_id(&self, id: u64) -> Result<Option<StackState>, PristineError>;

    /// Resolve the overlay chain for an local workspace.
    ///
    /// Walks the `parent` links from the given stack upward, collecting
    /// the IDs of each **Local** ancestor. Stops when a **Shared**
    /// ancestor (or the root) is reached, since Shared stacks read from
    /// the global `GRAPH`.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to resolve the chain for
    ///
    /// # Returns
    ///
    /// A vector of stack IDs representing the overlay chain, ordered from
    /// the given stack (most specific) to the last Local ancestor.
    /// The global `GRAPH` is implicitly the base and is not included.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // feature-login (Local, parent=service-auth)
    /// // service-auth  (Local, parent=dev)
    /// // dev           (Shared,   parent=main)
    ///
    /// let chain = txn.resolve_overlay_chain(&feature_login)?;
    /// // chain = [feature_login.id, service_auth.id]
    /// // GRAPH is the implicit base (dev is Shared → stop)
    /// ```
    fn resolve_overlay_chain(&self, stack: &StackState) -> Result<Vec<u64>, PristineError> {
        let mut chain: Vec<u64> = Vec::new();

        if stack.kind.is_shared() {
            // Shared stacks read directly from GRAPH, no overlay needed
            return Ok(chain);
        }

        chain.push(stack.id);

        let mut cursor = stack.parent;
        while let Some(parent_id) = cursor {
            let parent = self.get_stack_by_id(parent_id)?;
            match parent {
                Some(p) if p.kind.is_local() => {
                    chain.push(p.id);
                    cursor = p.parent;
                }
                _ => break, // Shared ancestor or not found → GRAPH is the base
            }
        }

        Ok(chain)
    }

    /// Iterate over edges in the stack-scoped graph for a given vertex.
    ///
    /// Returns edges from `STACK_GRAPH[(stack_id, vertex)]` that have flags
    /// within the specified range. This is the per-stack equivalent of
    /// [`GraphTxnT::iter_adjacent`] which reads from the global `GRAPH`.
    ///
    /// # Arguments
    ///
    /// * `stack_id` - The local workspace's internal ID
    /// * `node` - The source vertex
    /// * `min_flag` - Minimum edge flags (inclusive)
    /// * `max_flag` - Maximum edge flags (inclusive)
    ///
    /// # Returns
    ///
    /// An iterator yielding edges that match the flag criteria.
    fn iter_stack_graph_adjacent(
        &self,
        stack_id: u64,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<
        Box<dyn Iterator<Item = Result<SerializedGraphEdge, PristineError>> + '_>,
        PristineError,
    >;

    /// Find all stacks that have the given stack as their parent.
    ///
    /// This is used during stack deletion to check for child stacks that
    /// would be orphaned. A stack with children cannot be deleted without
    /// first reparenting or deleting its children (unless `--force` is used).
    ///
    /// # Arguments
    ///
    /// * `parent_id` - The internal ID of the parent stack
    ///
    /// # Returns
    ///
    /// A vector of `StackState` for all stacks whose `parent == Some(parent_id)`.
    fn get_children_stacks(&self, parent_id: u64) -> Result<Vec<StackState>, PristineError> {
        // Default implementation: scan all stacks and filter by parent.
        // Stack counts are typically small (<100), so this is fine.
        let names = self.list_stacks()?;
        let mut children = Vec::new();
        for name in names {
            if let Some(stack) = self.get_stack(&name)? {
                if stack.parent == Some(parent_id) {
                    children.push(stack);
                }
            }
        }
        Ok(children)
    }

    /// Collect all unique vertex `(start, end)` positions for a given change
    /// within a stack's `STACK_GRAPH`.
    ///
    /// This performs a range scan on the `STACK_GRAPH` table to find all
    /// vertices belonging to a specific change in a specific stack. It is
    /// used by [`OverlayTxn`] to implement `find_block` and `find_block_end`
    /// against the `STACK_GRAPH`.
    ///
    /// # Arguments
    ///
    /// * `stack_id` - The local workspace's internal ID
    /// * `change_id` - The change whose vertices to collect
    ///
    /// # Returns
    ///
    /// A vector of `(start, end)` pairs representing vertex byte ranges.
    /// The pairs are deduplicated but not sorted in any guaranteed order.
    fn iter_stack_graph_vertices_for_change(
        &self,
        stack_id: u64,
        change_id: u64,
    ) -> Result<Vec<(u64, u64)>, PristineError>;

    /// Get a stack by name
    ///
    /// Looks up a stack by its human-readable name.
    ///
    /// # Arguments
    ///
    /// * `name` - The stack name to look up
    ///
    /// # Returns
    ///
    /// * `Ok(Some(state))` - The stack exists
    /// * `Ok(None)` - No stack with this name
    /// * `Err(_)` - Database error
    fn get_stack(&self, name: &str) -> Result<Option<StackState>, PristineError>;

    /// List all stack names
    ///
    /// Returns a vector of all stack names in the repository.
    /// The order is not guaranteed.
    ///
    /// # Returns
    ///
    /// A vector of stack names, or an empty vector if no stacks exist.
    fn list_stacks(&self) -> Result<Vec<String>, PristineError>;

    /// Get the current Merkle state for a stack
    ///
    /// This is a convenience method that extracts the state from a StackState.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to get the state of
    ///
    /// # Returns
    ///
    /// The current Merkle state hash.
    fn stack_state(&self, stack: &StackState) -> Merkle {
        stack.state
    }

    /// Get the sequence number for a change in a stack
    ///
    /// Looks up when (at what sequence number) a change was applied to this stack.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to search
    /// * `change_id` - The change to look up
    ///
    /// # Returns
    ///
    /// * `Ok(Some(seq))` - The change is in the stack at this sequence
    /// * `Ok(None)` - The change is not in this stack
    /// * `Err(_)` - Database error
    fn get_change_seq(
        &self,
        stack: &StackState,
        change_id: NodeId,
    ) -> Result<Option<u64>, PristineError>;

    /// Get the change at a sequence number in a stack
    ///
    /// Returns the change that was applied at a specific sequence number.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to search
    /// * `seq` - The sequence number (0-indexed)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(id))` - The change at this sequence
    /// * `Ok(None)` - No change at this sequence (out of range)
    /// * `Err(_)` - Database error
    fn get_change_at_seq(
        &self,
        stack: &StackState,
        seq: u64,
    ) -> Result<Option<NodeId>, PristineError>;

    /// Iterate over changes in a stack
    ///
    /// Returns an iterator over (sequence, change_id, merkle_state) tuples,
    /// starting from the given sequence number.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to iterate
    /// * `from_seq` - Starting sequence number (inclusive)
    ///
    /// # Returns
    ///
    /// An iterator yielding tuples of (sequence, change_id, merkle_at_that_point).
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Get all changes after sequence 10
    /// for result in txn.iter_changes(&stack, 10)? {
    ///     let (seq, change_id, state) = result?;
    ///     println!("Change #{}: {:?} (state: {})", seq, change_id, state);
    /// }
    /// ```
    fn iter_changes(
        &self,
        stack: &StackState,
        from_seq: u64,
    ) -> Result<
        Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>,
        PristineError,
    >;
}

// TreeTxnT - File Tree Operations

/// File tree operations
///
/// This trait provides access to the file tree mappings that connect:
/// - File paths ↔ Inodes (stable file identifiers)
/// - Inodes ↔ Graph positions (where the file's content lives in the graph)
///
/// # Why Inodes?
///
/// Inodes provide a stable identifier for files that survives renames. When
/// you rename a file, the inode stays the same—only the path→inode mapping
/// changes. This is crucial for tracking file history across renames.
///
/// # The Inode Graph Index
///
/// The `iter_inode_vertices` method uses a secondary index (INODE_GRAPH) that
/// allows O(n) iteration over a file's content, where n is the file size.
/// Without this index, you'd need to scan the entire graph (O(N) where N is
/// total repository size).
///
/// ```text
/// Path "src/main.rs"
///        │
///        ▼
///    Inode 42
///        │
///        ├── Position (change: 5, pos: 100)  ──▶  Vertices in INODE_GRAPH[42]
///        │                                           │
///        │                                           ▼
///        │                                    ┌─────────────────┐
///        │                                    │  File content   │
///        │                                    │  as a subgraph  │
///        │                                    └─────────────────┘
/// ```
///
/// # Example
///
/// ```ignore
/// fn read_file<T: TreeTxnT>(txn: &T, path: &str) -> PristineResult<Option<Vec<u8>>> {
///     // Look up the inode
///     let inode = match txn.get_inode(path)? {
///         Some(i) => i,
///         None => return Ok(None),
///     };
///
///     // Iterate over the file's vertices
///     let mut content = Vec::new();
///     for result in txn.iter_inode_vertices(inode)? {
///         let (span, edge) = result?;
///         // ... collect content from span ...
///     }
///
///     Ok(Some(content))
/// }
/// ```
pub trait TreeTxnT: GraphTxnT {
    /// Get the inode for a path
    ///
    /// Looks up the stable file identifier for a given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path (relative to repository root)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(inode))` - The file exists
    /// * `Ok(None)` - No file at this path
    /// * `Err(_)` - Database error
    fn get_inode(&self, path: &str) -> Result<Option<Inode>, PristineError>;

    /// Get directory flags for an inode.
    ///
    /// Checks if an inode represents a directory and returns its flags.
    /// This is a read-only operation available to both read and write transactions.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode to check
    ///
    /// # Returns
    ///
    /// * `Ok(Some(flags))` - The inode is a directory with these flags
    /// * `Ok(None)` - The inode is not a directory (it's a file)
    /// * `Err(_)` - Database error
    ///
    /// # Directory Flags
    ///
    /// See `directory_flags` module for flag constants:
    /// - `DIR_EXPLICIT` (0x01): Directory was explicitly tracked
    /// - `DIR_EMPTY` (0x02): Directory has no tracked children
    ///
    /// # Example
    ///
    /// ```ignore
    /// use atomic_core::pristine::{TreeTxnT, directory_flags};
    ///
    /// if let Some(flags) = txn.get_directory_flags(inode)? {
    ///     if directory_flags::is_empty(flags) {
    ///         println!("Empty directory");
    ///     }
    /// }
    /// ```
    fn get_directory_flags(&self, inode: Inode) -> Result<Option<u8>, PristineError>;

    /// Check if an inode represents a directory.
    ///
    /// Convenience method that returns `true` if the inode is marked as a
    /// directory in the DIRECTORIES table.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode to check
    ///
    /// # Returns
    ///
    /// `true` if this inode is a directory, `false` if it's a file.
    fn is_directory(&self, inode: Inode) -> Result<bool, PristineError> {
        Ok(self.get_directory_flags(inode)?.is_some())
    }

    /// Get the path for an inode
    ///
    /// Returns the current path for a file identified by inode.
    /// This is the inverse of `get_inode`.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file's stable identifier
    ///
    /// # Returns
    ///
    /// * `Ok(Some(path))` - The inode has a path
    /// * `Ok(None)` - The inode doesn't exist or has no path
    /// * `Err(_)` - Database error
    fn get_path(&self, inode: Inode) -> Result<Option<String>, PristineError>;

    /// Get the graph position for an inode
    ///
    /// Returns the position in the graph where this file's content root is.
    /// This is the entry point for traversing the file's content graph.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file's stable identifier
    ///
    /// # Returns
    ///
    /// * `Ok(Some(pos))` - The file's root position in the graph
    /// * `Ok(None)` - The inode has no graph position
    /// * `Err(_)` - Database error
    fn inode_position(&self, inode: Inode) -> Result<Option<Position<NodeId>>, PristineError>;

    /// Get the inode for a graph position
    ///
    /// Returns the inode that contains this position.
    /// This is the inverse of `inode_position`.
    ///
    /// # Arguments
    ///
    /// * `pos` - A position in the graph
    ///
    /// # Returns
    ///
    /// * `Ok(Some(inode))` - The inode containing this position
    /// * `Ok(None)` - No inode at this position
    /// * `Err(_)` - Database error
    fn position_inode(&self, pos: Position<NodeId>) -> Result<Option<Inode>, PristineError>;

    /// Iterate over all files in the tree
    ///
    /// Returns an iterator over (path, inode) pairs for all tracked files.
    ///
    /// # Returns
    ///
    /// An iterator yielding (path, inode) tuples.
    ///
    /// # Note
    ///
    /// The order of iteration is not guaranteed.
    fn iter_tree(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>, PristineError>;

    /// Iterate over vertices for a specific inode
    ///
    /// Uses the inode graph index for O(n) file traversal where n is the
    /// file size in vertices. This is much more efficient than scanning
    /// the entire graph.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file to iterate
    ///
    /// # Returns
    ///
    /// An iterator yielding (span, edge) pairs for the file's content.
    ///
    /// # Performance
    ///
    /// This uses the INODE_GRAPH secondary index, providing O(m) complexity
    /// where m is the number of vertices in the file, rather than O(N) where
    /// N is the total graph size.
    fn iter_inode_vertices(
        &self,
        inode: Inode,
    ) -> Result<
        Box<
            dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                + '_,
        >,
        PristineError,
    >;

    /// Get the cached file metadata (mtime + size) for a tracked file.
    ///
    /// Returns the filesystem metadata snapshot taken at the time the file
    /// was last recorded or applied. During status, if the current `stat()`
    /// values match, we skip the expensive graph content comparison.
    ///
    /// # Arguments
    ///
    /// * `path` - File path (relative to repository root)
    ///
    /// # Returns
    ///
    /// * `Ok(Some((mtime_secs, mtime_nanos, file_size)))` - Cached metadata
    /// * `Ok(None)` - No cached metadata for this path
    /// * `Err(_)` - Database error
    fn get_file_mtime(&self, path: &str) -> Result<Option<(i64, u32, u64)>, PristineError>;
}

// MutTxnT - Mutable Operations

/// Mutable graph operations
///
/// This trait extends the read traits with write operations. It provides
/// the full API needed to modify the repository state.
///
/// # Transaction Lifecycle
///
/// Write transactions must be explicitly committed or aborted:
///
/// ```ignore
/// let mut txn = pristine.write_txn()?;
///
/// // Make changes...
/// txn.open_or_create_stack("feature")?;
///
/// // Either commit:
/// txn.commit()?;
///
/// // Or abort (rolls back all changes):
/// // txn.abort()?;
/// ```
///
/// If a `WriteTxn` is dropped without calling `commit()` or `abort()`,
/// the transaction is automatically aborted.
///
/// # Atomicity
///
/// All operations within a transaction are atomic—either all succeed and
/// are committed, or none take effect. This ensures the database is always
/// in a consistent state.
///
/// # Example
///
/// ```ignore
/// fn add_file<T: MutTxnT>(
///     txn: &mut T,
///     stack: &mut StackState,
///     path: &str,
///     content: &[u8],
/// ) -> PristineResult<()> {
///     // Allocate an inode for the file
///     let inode = txn.alloc_inode()?;
///
///     // Add to tree
///     txn.put_tree(path, inode)?;
///
///     // Create position in graph (simplified)
///     let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
///     txn.put_inode(inode, pos)?;
///
///     // ... add vertices and edges for content ...
///
///     Ok(())
/// }
/// ```
pub trait MutTxnT: StackTxnT + TreeTxnT {
    // Change Registration

    /// Register a new internal ID for an external hash
    ///
    /// Creates a mapping between an external content hash and an internal
    /// repository-local ID. If the hash is already registered, returns the
    /// existing ID.
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash to register
    ///
    /// # Returns
    ///
    /// The internal NodeId for this hash (existing or newly allocated).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let hash = Hash::of(b"change content...");
    /// let node_id = txn.register_change(&hash)?;
    ///
    /// // Registering again returns the same ID
    /// let node_id2 = txn.register_change(&hash)?;
    /// assert_eq!(node_id, node_id2);
    /// ```
    fn register_change(&mut self, hash: &Hash) -> Result<NodeId, PristineError>;

    /// Register a new internal ID for a tag hash
    ///
    /// Creates a mapping between a tag's content hash and an internal
    /// repository-local ID. If the hash is already registered, returns the
    /// existing ID. Tags are differentiated from changes by their node type.
    ///
    /// # Arguments
    ///
    /// * `hash` - The tag content hash to register
    ///
    /// # Returns
    ///
    /// The internal NodeId for this tag (existing or newly allocated).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let hash = Hash::of(b"tag content...");
    /// let node_id = txn.register_tag(&hash)?;
    ///
    /// // Check the node type
    /// let node_type = txn.get_node_type(node_id)?;
    /// assert_eq!(node_type, Some(node_type::TAG));
    /// ```
    fn register_tag(&mut self, hash: &Hash) -> Result<NodeId, PristineError>;

    /// Register an attestation in the graph.
    ///
    /// Attestations are graph-level audit nodes that capture metadata about
    /// a set of changes (cost, tokens, model usage, duration). They are
    /// content-addressed like changes and tags but produce zero hunks —
    /// they don't modify the content graph.
    ///
    /// Attestations are NOT added to any stack's changelog. They live in
    /// the graph as standalone nodes with dependencies (DEPS) pointing to
    /// the changes they cover.
    ///
    /// # Arguments
    ///
    /// * `hash` - The Blake3 hash of the serialized attestation
    ///
    /// # Returns
    ///
    /// The internal `NodeId` assigned to this attestation.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let node_id = txn.register_attestation(&hash)?;
    ///
    /// // Check the node type
    /// let node_type = txn.get_node_type(node_id)?;
    /// assert_eq!(node_type, Some(node_type::ATTESTATION));
    /// ```
    fn register_attestation(&mut self, hash: &Hash) -> Result<NodeId, PristineError>;

    /// Register a provenance graph and get its internal ID.
    ///
    /// Similar to [`register_change`] and [`register_attestation`], but
    /// for provenance graph artifacts. Uses `node_type::PROVENANCE`.
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash of the provenance graph
    ///
    /// # Returns
    ///
    /// The internal `NodeId` assigned to this provenance graph.
    fn register_provenance(&mut self, hash: &Hash) -> Result<NodeId, PristineError>;

    // Graph Modification

    /// Add an edge to the graph
    ///
    /// Inserts an edge from a span. If the edge already exists, this is a no-op.
    ///
    /// # Arguments
    ///
    /// * `span` - The source span
    /// * `edge` - The edge to add
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The edge was newly inserted
    /// * `Ok(false)` - The edge already existed
    /// * `Err(_)` - Database error
    fn put_graph(
        &mut self,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    /// Remove an edge from the graph
    ///
    /// Deletes an edge from a span.
    ///
    /// # Arguments
    ///
    /// * `span` - The source span
    /// * `edge` - The edge to remove
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The edge was removed
    /// * `Ok(false)` - The edge didn't exist
    /// * `Err(_)` - Database error
    fn del_graph(
        &mut self,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    /// Add an edge to the inode graph index
    ///
    /// This maintains the secondary index for efficient per-file traversal.
    /// Should be called whenever an edge is added that's part of a file.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file this edge belongs to
    /// * `span` - The source span
    /// * `edge` - The edge to index
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The index entry was newly inserted
    /// * `Ok(false)` - The entry already existed
    /// * `Err(_)` - Database error
    fn put_inode_graph(
        &mut self,
        inode: Inode,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    /// Remove an edge from the inode graph index
    ///
    /// Maintains the secondary index when edges are removed.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file this edge belongs to
    /// * `span` - The source span
    /// * `edge` - The edge to remove from the index
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The index entry was removed
    /// * `Ok(false)` - The entry didn't exist
    /// * `Err(_)` - Database error
    fn del_inode_graph(
        &mut self,
        inode: Inode,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    // Stack Graph Operations (Local Workspace Edge Storage)

    /// Add an edge to the stack-scoped graph.
    ///
    /// This stores edges for **Local** stacks in the `STACK_GRAPH` table,
    /// keyed by `(stack_id, vertex)`. These edges are only visible through
    /// the overlay chain and are cascade-deleted when the stack is removed.
    ///
    /// # Arguments
    ///
    /// * `stack_id` - The local workspace's internal ID
    /// * `node` - The source vertex
    /// * `edge` - The edge to add
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The edge was newly inserted
    /// * `Ok(false)` - The edge already existed
    /// * `Err(_)` - Database error
    fn put_stack_graph(
        &mut self,
        stack_id: u64,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    /// Remove an edge from the stack-scoped graph.
    ///
    /// # Arguments
    ///
    /// * `stack_id` - The local workspace's internal ID
    /// * `node` - The source vertex
    /// * `edge` - The edge to remove
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The edge was removed
    /// * `Ok(false)` - The edge didn't exist
    /// * `Err(_)` - Database error
    fn del_stack_graph(
        &mut self,
        stack_id: u64,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    /// Cascade-delete all edges for an local workspace.
    ///
    /// Removes every entry in `STACK_GRAPH` whose key starts with `stack_id`.
    /// This is called during `del_stack` for Local workspaces to ensure zero
    /// orphaned edges remain in the graph.
    ///
    /// # Arguments
    ///
    /// * `stack_id` - The local workspace's internal ID
    ///
    /// # Returns
    ///
    /// The number of edges deleted.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Delete all pending edges for the feature stack
    /// let count = txn.del_stack_graph_prefix(feature_stack.id)?;
    /// println!("Removed {} orphaned edges", count);
    /// ```
    fn del_stack_graph_prefix(&mut self, stack_id: u64) -> Result<u64, PristineError>;

    // Stack Operations

    /// Open or create a stack
    ///
    /// If a stack with the given name exists, returns it. Otherwise creates
    /// a new stack with zero changes.
    ///
    /// # Arguments
    ///
    /// * `name` - The stack name
    ///
    /// # Returns
    ///
    /// The stack state (existing or newly created).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stack = txn.open_or_create_stack("feature-x")?;
    /// println!("Stack has {} changes", stack.change_count);
    /// ```
    fn open_or_create_stack(&mut self, name: &str) -> Result<StackState, PristineError>;

    /// Create a new stack with explicit kind and parent.
    ///
    /// If a stack with the given name already exists, returns an error.
    /// Use [`open_or_create_stack`] for the backward-compatible "get or create"
    /// behavior (which defaults to Shared, no parent).
    ///
    /// # Arguments
    ///
    /// * `name` - The stack name (must be unique)
    /// * `kind` - Whether this stack is Local or Shared
    /// * `parent` - The parent stack's ID (`None` only for the root stack)
    ///
    /// # Errors
    ///
    /// - `PristineError::StackAlreadyExists` if a stack with this name exists
    /// - `PristineError::StackNotFound` if `parent` references a non-existent stack
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Create a shared "dev" stack parented on "main" (id=1)
    /// let dev = txn.create_stack("dev", StackKind::Shared, Some(1))?;
    ///
    /// // Create a local "feature" stack parented on "dev"
    /// let feature = txn.create_stack("feature", StackKind::Local, Some(dev.id))?;
    /// ```
    fn create_stack(
        &mut self,
        name: &str,
        kind: StackKind,
        parent: Option<u64>,
    ) -> Result<StackState, PristineError>;

    /// Look up a stack by its internal ID.
    ///
    /// This is used to resolve parent references when walking the overlay
    /// chain. Unlike [`StackTxnT::get_stack`] which looks up by name, this
    /// looks up by the internal numeric ID stored in `StackState::id`.
    ///
    /// # Arguments
    ///
    /// * `id` - The internal stack identifier
    ///
    /// # Returns
    ///
    /// * `Ok(Some(state))` - The stack with this ID
    /// * `Ok(None)` - No stack with this ID
    /// * `Err(_)` - Database error
    ///
    /// # Note
    ///
    /// This method also exists on [`StackTxnT`] for read-only access.
    /// The `MutTxnT` version delegates to the `StackTxnT` implementation.
    fn get_stack_by_id(&self, id: u64) -> Result<Option<StackState>, PristineError> {
        // Default implementation delegates to StackTxnT (which MutTxnT: StackTxnT)
        StackTxnT::get_stack_by_id(self, id)
    }

    /// Record a change in a stack
    ///
    /// Appends the change to the stack's log and updates the Merkle state.
    /// This is how changes are "applied" to a stack.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to modify (will be updated in place)
    /// * `change_id` - The internal ID of the change
    /// * `change_hash` - The content hash of the change
    ///
    /// # Returns
    ///
    /// The sequence number assigned to this change in the stack.
    ///
    /// # Side Effects
    ///
    /// - Updates `stack.state` with the new Merkle hash
    /// - Increments `stack.change_count`
    /// - Records the state in the TAGS table
    /// - Records state→sequence mapping in STATES table
    ///
    /// # Example
    ///
    /// ```ignore
    /// let hash = Hash::of(b"change content");
    /// let change_id = txn.register_change(&hash)?;
    /// let seq = txn.put_change(&mut stack, change_id, &hash)?;
    /// println!("Change applied at sequence {}", seq);
    /// ```
    fn put_change(
        &mut self,
        stack: &mut StackState,
        change_id: NodeId,
        change_hash: &Hash,
    ) -> Result<u64, PristineError>;

    /// Remove a change from a stack (unrecord).
    ///
    /// This removes a change from the stack's view without deleting the change
    /// itself. The change remains in the graph and can be re-applied later.
    /// This is similar to Gerrit's workflow where a patch can be removed from
    /// a change set, modified, and re-inserted.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to remove the change from
    /// * `change_id` - Internal ID of the change to remove
    /// * `change_hash` - Hash of the change (for merkle recomputation)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(seq))` - The sequence number where the change was removed from
    /// * `Ok(None)` - The change was not in this stack
    /// * `Err(_)` - Database error
    ///
    /// # Notes
    ///
    /// After calling this method, you must call `update_stack` to persist the
    /// changes. The stack's merkle state will be recomputed to exclude this
    /// change.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Remove the last change from the stack
    /// if let Some(seq) = txn.del_change(&mut stack, change_id, &hash)? {
    ///     println!("Removed change from sequence {}", seq);
    ///     txn.update_stack(&stack)?;
    /// }
    /// ```
    fn del_change(
        &mut self,
        stack: &mut StackState,
        change_id: NodeId,
        change_hash: &Hash,
    ) -> Result<Option<u64>, PristineError>;

    /// Reinsert a previously unrecorded change at a specific position.
    ///
    /// This is part of the Gerrit-like workflow where a change can be removed,
    /// modified, and re-inserted at its original position (or a new position).
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to insert the change into
    /// * `change_id` - Internal ID of the change to insert
    /// * `change_hash` - Hash of the change
    /// * `at_sequence` - The sequence position to insert at (shifts later changes)
    ///
    /// # Returns
    ///
    /// * `Ok(())` - The change was inserted successfully
    /// * `Err(_)` - Database error or invalid sequence
    ///
    /// # Notes
    ///
    /// - If `at_sequence` is beyond the current change count, the change is
    ///   appended to the end.
    /// - Changes after the insertion point have their sequence numbers shifted.
    /// - The stack's merkle state is recomputed from scratch.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Re-insert a change at its original position
    /// txn.reinsert_change(&mut stack, change_id, &hash, original_seq)?;
    /// txn.update_stack(&stack)?;
    /// ```
    fn reinsert_change(
        &mut self,
        stack: &mut StackState,
        change_id: NodeId,
        change_hash: &Hash,
        at_sequence: u64,
    ) -> Result<(), PristineError>;

    /// Update the stack state after modifications
    ///
    /// Persists the stack's current state to the database. Call this after
    /// modifying a stack with `put_change`.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to persist
    ///
    /// # Example
    ///
    /// ```ignore
    /// txn.put_change(&mut stack, change_id, &hash)?;
    /// txn.update_stack(&stack)?;  // Persist the changes
    /// ```
    fn update_stack(&mut self, stack: &StackState) -> Result<(), PristineError>;

    /// Delete a stack from the database.
    ///
    /// For **Local** stacks, this cascade-deletes all edges from
    /// `STACK_GRAPH[(stack_id, *)]` and then removes all metadata:
    /// - `STACK_GRAPH` edges (cascade prefix delete — zero orphans)
    /// - Stack metadata from `STACKS` table
    /// - Change log entries from `STACK_CHANGES` table
    /// - Reverse change log entries from `REV_STACK_CHANGES` table
    /// - State/sequence mappings from `STATES` table
    /// - Tag entries from `TAGS` table
    ///
    /// **Shared** stacks cannot be deleted because their edges live in the
    /// global `GRAPH` table and are depended on by all stacks. Attempting
    /// to delete a Shared stack returns `PristineError::CannotDeleteSharedStack`.
    ///
    /// A stack that has **child stacks** (other stacks with `parent == this.id`)
    /// cannot be deleted — returns `PristineError::StackHasChildren`. Delete
    /// or reparent children first.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to delete (must be Local, with no children)
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Stack and all its STACK_GRAPH edges were deleted
    /// * `Err(CannotDeleteSharedStack)` - Stack is Shared
    /// * `Err(StackHasChildren)` - Stack has child stacks
    /// * `Err(_)` - Database error
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stack = txn.get_stack("feature-branch")?.unwrap();
    /// // Only works for Local workspaces with no children
    /// txn.del_stack(&stack)?;
    /// txn.commit()?;
    /// ```
    fn del_stack(&mut self, stack: &StackState) -> Result<(), PristineError>;

    // Tree Operations

    /// Add a file to the tree
    ///
    /// Creates both path→inode and inode→path mappings.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path (relative to repository root)
    /// * `inode` - The file's inode
    fn put_tree(&mut self, path: &str, inode: Inode) -> Result<(), PristineError>;

    /// Remove a file from the tree
    ///
    /// Removes both path→inode and inode→path mappings.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path to remove
    ///
    /// # Returns
    ///
    /// The inode that was removed, if any.
    fn del_tree(&mut self, path: &str) -> Result<Option<Inode>, PristineError>;

    /// Store file metadata (mtime + size) for fast status detection.
    ///
    /// Called after a file is recorded or applied. Stores the filesystem
    /// metadata so that subsequent `status()` calls can skip the expensive
    /// graph content comparison for unchanged files.
    ///
    /// # Arguments
    ///
    /// * `path` - File path (relative to repository root)
    /// * `mtime_secs` - Modification time (seconds since epoch)
    /// * `mtime_nanos` - Modification time (nanoseconds component)
    /// * `file_size` - File size in bytes
    fn put_file_mtime(
        &mut self,
        path: &str,
        mtime_secs: i64,
        mtime_nanos: u32,
        file_size: u64,
    ) -> Result<(), PristineError>;

    /// Remove cached file metadata.
    ///
    /// Called when a file is deleted or untracked.
    ///
    /// # Arguments
    ///
    /// * `path` - File path to remove from the mtime cache
    fn del_file_mtime(&mut self, path: &str) -> Result<(), PristineError>;

    /// Map an inode to a graph position
    ///
    /// Creates both inode→position and position→inode mappings.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file's inode
    /// * `pos` - The root position in the graph
    fn put_inode(&mut self, inode: Inode, pos: Position<NodeId>) -> Result<(), PristineError>;

    // Directory Operations

    /// Mark an inode as a directory.
    ///
    /// This records that the given inode represents a directory rather than
    /// a file. Directories can be explicitly tracked even when empty.
    ///
    /// # Arguments
    ///
    /// * `inode` - The directory's inode
    /// * `flags` - Directory flags (see `directory_flags` module)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use atomic_core::pristine::tables::directory_flags;
    ///
    /// // Mark as explicitly tracked empty directory
    /// txn.put_directory(inode, directory_flags::explicit_empty())?;
    /// ```
    fn put_directory(&mut self, inode: Inode, flags: u8) -> Result<(), PristineError>;

    /// Remove the directory marker from an inode.
    ///
    /// # Arguments
    ///
    /// * `inode` - The directory's inode
    ///
    /// # Returns
    ///
    /// The flags that were set, if any.
    fn del_directory(&mut self, inode: Inode) -> Result<Option<u8>, PristineError>;

    /// Update directory flags.
    ///
    /// This is used to update flags when directory contents change
    /// (e.g., marking as non-empty when a file is added).
    ///
    /// # Arguments
    ///
    /// * `inode` - The directory's inode
    /// * `flags` - New flags to set
    fn update_directory_flags(&mut self, inode: Inode, flags: u8) -> Result<(), PristineError> {
        // Default implementation: delete and re-add
        self.del_directory(inode)?;
        self.put_directory(inode, flags)
    }

    /// Remove an inode mapping
    ///
    /// Removes both inode→position and position→inode mappings.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode to unmap
    ///
    /// # Returns
    ///
    /// The position that was mapped, if any.
    fn del_inode(&mut self, inode: Inode) -> Result<Option<Position<NodeId>>, PristineError>;

    // Dependency Operations

    /// Add a dependency relationship
    ///
    /// Records that `change_id` depends on `dep_id`. This is used for
    /// dependency tracking and ensuring changes are applied in valid order.
    ///
    /// # Arguments
    ///
    /// * `change_id` - The change that has the dependency
    /// * `dep_id` - The change being depended upon
    fn put_dep(&mut self, change_id: NodeId, dep_id: NodeId) -> Result<(), PristineError>;

    /// Get dependencies of a change
    ///
    /// Returns all changes that the given change depends on.
    ///
    /// # Arguments
    ///
    /// * `change_id` - The change to get dependencies for
    ///
    /// # Returns
    ///
    /// A vector of NodeIds this change depends on.
    fn get_deps(&self, change_id: NodeId) -> Result<Vec<NodeId>, PristineError>;

    // Allocation

    /// Allocate a new inode
    ///
    /// Returns a unique inode identifier for a new file.
    ///
    /// # Returns
    ///
    /// A newly allocated, unique Inode.
    ///
    /// # Thread Safety
    ///
    /// This uses atomic operations and is safe to call from multiple
    /// transactions (though only one write transaction can be active).
    fn alloc_inode(&mut self) -> Result<Inode, PristineError>;

    // CRDT Table Operations

    /// Store a trunk (file) entry in the CRDT tables.
    ///
    /// # Arguments
    ///
    /// * `key` - Encoded TrunkId (12 bytes)
    /// * `value` - Encoded SerializedTrunk
    fn put_crdt_trunk(&mut self, key: &[u8; 12], value: &[u8]) -> Result<(), PristineError>;

    /// Get a trunk entry from the CRDT tables.
    ///
    /// # Arguments
    ///
    /// * `key` - Encoded TrunkId (12 bytes)
    ///
    /// # Returns
    ///
    /// The deserialized trunk, if it exists.
    fn get_crdt_trunk(
        &mut self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedTrunk>, PristineError>;

    /// Store an inode→trunk mapping.
    fn put_crdt_inode_trunk(
        &mut self,
        inode: u64,
        trunk_key: &[u8; 12],
    ) -> Result<(), PristineError>;

    /// Store a path→trunk mapping.
    fn put_crdt_path_trunk(
        &mut self,
        path: &str,
        trunk_key: &[u8; 12],
    ) -> Result<(), PristineError>;

    /// Remove a path→trunk mapping.
    fn del_crdt_path_trunk(&mut self, path: &str) -> Result<(), PristineError>;

    /// Store a branch (line) entry in the CRDT tables.
    ///
    /// # Arguments
    ///
    /// * `key` - Encoded BranchId (12 bytes)
    /// * `value` - Encoded SerializedBranch (24 bytes)
    fn put_crdt_branch(&mut self, key: &[u8; 12], value: &[u8; 24]) -> Result<(), PristineError>;

    /// Get a branch entry from the CRDT tables.
    fn get_crdt_branch(
        &mut self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedBranch>, PristineError>;

    /// Add a branch to a trunk's branch list (multimap).
    fn put_crdt_trunk_branch(
        &mut self,
        trunk_key: &[u8; 12],
        branch_key: &[u8; 12],
    ) -> Result<(), PristineError>;

    /// Store a leaf (token) entry in the CRDT tables.
    ///
    /// # Arguments
    ///
    /// * `key` - Encoded LeafId (12 bytes)
    /// * `value` - Encoded SerializedLeaf (22 bytes)
    fn put_crdt_leaf(&mut self, key: &[u8; 12], value: &[u8; 22]) -> Result<(), PristineError>;

    /// Get a leaf entry from the CRDT tables.
    fn get_crdt_leaf(
        &mut self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedLeaf>, PristineError>;

    /// Add a leaf to a branch's leaf list (multimap).
    fn put_crdt_branch_leaf(
        &mut self,
        branch_key: &[u8; 12],
        leaf_key: &[u8; 12],
    ) -> Result<(), PristineError>;

    /// Look up a trunk by file path.
    ///
    /// Returns the TrunkId for the file at the given path, if it exists.
    fn get_trunk_by_path(
        &mut self,
        path: &str,
    ) -> Result<Option<crate::crdt::TrunkId>, PristineError>;

    /// Iterate over all branches (lines) belonging to a trunk (file).
    ///
    /// Returns branch IDs in CRDT ordering (by BranchId).
    fn iter_trunk_branches(
        &mut self,
        trunk_key: &[u8; 12],
    ) -> Result<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>, PristineError>;

    /// Iterate over all leaves (tokens) belonging to a branch (line).
    ///
    /// Returns leaf IDs in CRDT ordering (by LeafId).
    fn iter_branch_leaves(
        &mut self,
        branch_key: &[u8; 12],
    ) -> Result<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>, PristineError>;

    /// Store a branch→span mapping for CRDT graph integration.
    ///
    /// This mapping allows finding the graph span when processing
    /// delete operations, which is necessary to mark edges with DELETED flags.
    ///
    /// # Arguments
    ///
    /// * `branch_key` - Encoded BranchId (12 bytes)
    /// * `span` - Encoded Span position (24 bytes)
    fn put_crdt_branch_vertex(
        &mut self,
        branch_key: &[u8; 12],
        node_bytes: &[u8; 24],
    ) -> Result<(), PristineError>;

    /// Get the graph span for a branch.
    ///
    /// Returns the span position that was stored when the branch was
    /// first inserted, enabling delete operations to find and mark the
    /// corresponding graph edges.
    fn get_crdt_branch_vertex(
        &mut self,
        branch_key: &[u8; 12],
    ) -> Result<Option<crate::types::GraphNode<NodeId>>, PristineError>;

    /// Store inode→position mapping for CRDT compatibility.
    fn put_inodes(&mut self, inode: u64, pos: &Position<NodeId>) -> Result<(), PristineError>;

    // Transaction Control

    /// Commit the transaction
    ///
    /// Persists all changes made in this transaction to the database.
    /// After commit, the transaction is consumed and cannot be used.
    ///
    /// # Errors
    ///
    /// Returns an error if the commit fails (e.g., disk full).
    /// On error, changes may or may not have been persisted.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut txn = pristine.write_txn()?;
    /// txn.open_or_create_stack("main")?;
    /// txn.commit()?;  // Persist changes
    /// ```
    fn commit(self) -> Result<(), PristineError>;

    /// Abort the transaction (rollback)
    ///
    /// Discards all changes made in this transaction.
    /// After abort, the transaction is consumed and cannot be used.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut txn = pristine.write_txn()?;
    /// txn.open_or_create_stack("main")?;
    /// txn.abort()?;  // Discard changes
    /// // Stack "main" was not created
    /// ```
    fn abort(self) -> Result<(), PristineError>;
}

// VertexExt - Convenience Trait

/// Extension trait for convenient span creation
///
/// Provides a helper method for creating vertices from their component parts.
///
/// # Example
///
/// ```
/// use atomic_core::pristine::VertexExt;
/// use atomic_core::types::{NodeId, GraphNode};
///
/// let node = GraphNode::from_parts(NodeId::new(42), 100, 200);
/// assert_eq!(node.change.get(), 42);
/// assert_eq!(node.start.get(), 100);
/// assert_eq!(node.end.get(), 200);
/// ```
pub trait VertexExt {
    /// Create a span from component parts
    ///
    /// # Arguments
    ///
    /// * `change_id` - The change that introduced this span
    /// * `start` - Start position (inclusive)
    /// * `end` - End position (exclusive)
    fn from_parts(change_id: NodeId, start: u64, end: u64) -> GraphNode<NodeId>;
}

impl VertexExt for GraphNode<NodeId> {
    fn from_parts(change_id: NodeId, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode {
            change: change_id,
            start: ChangePosition::new(start),
            end: ChangePosition::new(end),
        }
    }
}

// Tests

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::*;

    #[test]
    fn test_stack_state_default() {
        let state = StackState::default();
        assert_eq!(state.id, 0);
        assert_eq!(state.name, "");
        assert_eq!(state.state, Merkle::ZERO);
        assert_eq!(state.change_count, 0);
        assert_eq!(state.kind, StackKind::Shared);
        assert_eq!(state.parent, None);
    }

    #[test]
    fn test_stack_state_new() {
        let state = StackState::new(42, "test".to_string());
        assert_eq!(state.id, 42);
        assert_eq!(state.name, "test");
        assert_eq!(state.state, Merkle::ZERO);
        assert_eq!(state.change_count, 0);
        assert_eq!(state.kind, StackKind::Shared);
        assert_eq!(state.parent, None);
    }

    #[test]
    fn test_stack_state_with_kind() {
        let state = StackState::with_kind(3, "feature".to_string(), StackKind::Local, Some(2));
        assert_eq!(state.id, 3);
        assert_eq!(state.name, "feature");
        assert_eq!(state.kind, StackKind::Local);
        assert_eq!(state.parent, Some(2));
        assert!(state.is_empty());
        assert!(!state.is_root());
    }

    #[test]
    fn test_stack_state_is_empty() {
        let mut state = StackState::new(1, "test".to_string());
        assert!(state.is_empty());
        state.change_count = 1;
        assert!(!state.is_empty());
    }

    #[test]
    fn test_stack_state_is_root() {
        let root = StackState::new(1, "main".to_string());
        assert!(root.is_root());

        let child = StackState::with_kind(2, "dev".to_string(), StackKind::Shared, Some(1));
        assert!(!child.is_root());
    }

    #[test]
    fn test_stack_kind_from_u8() {
        assert_eq!(StackKind::from_u8(0), Some(StackKind::Local));
        assert_eq!(StackKind::from_u8(1), Some(StackKind::Shared));
        assert_eq!(StackKind::from_u8(2), None);
        assert_eq!(StackKind::from_u8(255), None);
    }

    #[test]
    fn test_stack_kind_display() {
        assert_eq!(format!("{}", StackKind::Local), "local");
        assert_eq!(format!("{}", StackKind::Shared), "shared");
    }

    #[test]
    fn test_stack_kind_default() {
        assert_eq!(StackKind::default(), StackKind::Shared);
    }

    #[test]
    fn test_stack_kind_predicates() {
        assert!(StackKind::Shared.is_shared());
        assert!(!StackKind::Shared.is_local());
        assert!(StackKind::Local.is_local());
        assert!(!StackKind::Local.is_shared());
    }

    #[test]
    fn test_vertex_from_parts() {
        let v = GraphNode::from_parts(NodeId::new(42), 100, 200);
        assert_eq!(v.change.get(), 42);
        assert_eq!(v.start.get(), 100);
        assert_eq!(v.end.get(), 200);
    }

    #[test]
    fn test_vertex_from_parts_empty() {
        let v = GraphNode::from_parts(NodeId::new(1), 50, 50);
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn test_vertex_from_parts_length() {
        let v = GraphNode::from_parts(NodeId::new(1), 10, 60);
        assert!(!v.is_empty());
        assert_eq!(v.len(), 50);
    }
}
