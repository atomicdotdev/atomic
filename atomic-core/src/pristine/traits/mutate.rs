//! Mutable graph operations trait.
//!
//! `MutTxnT` extends all read traits with write operations for modifying
//! the repository graph, file tree, views, and CRDT tables.

use crate::types::{GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge};

use crate::pristine::error::PristineError;

use super::tree::TreeTxnT;
use super::view::{ViewScope, ViewState, ViewTxnT};

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
/// txn.open_or_create_view("feature")?;
/// txn.commit()?;  // or txn.abort()?;
/// ```
///
/// If a `WriteTxn` is dropped without calling `commit()` or `abort()`,
/// the transaction is automatically aborted.
///
/// # Atomicity
///
/// All operations within a transaction are atomic—either all succeed and
/// are committed, or none take effect.
pub trait MutTxnT: ViewTxnT + TreeTxnT {
    // ── Change Registration ─────────────────────────────────────

    /// Register a new internal ID for an external hash.
    ///
    /// Creates a mapping between an external content hash and an internal
    /// repository-local ID. Returns the existing ID if already registered.
    fn register_change(&mut self, hash: &Hash) -> Result<NodeId, PristineError>;

    /// Register a new internal ID for a tag hash.
    ///
    /// Tags are differentiated from changes by their node type.
    fn register_tag(&mut self, hash: &Hash) -> Result<NodeId, PristineError>;

    /// Register an attestation in the graph.
    ///
    /// Attestations are audit nodes capturing metadata (cost, tokens, model
    /// usage) about a set of changes. They produce zero hunks and are not
    /// added to any view's changelog.
    fn register_attestation(&mut self, hash: &Hash) -> Result<NodeId, PristineError>;

    /// Register a provenance graph and get its internal ID.
    fn register_provenance(&mut self, hash: &Hash) -> Result<NodeId, PristineError>;

    // ── Graph Modification ──────────────────────────────────────

    /// Add an edge to the graph.
    ///
    /// Returns `Ok(true)` if newly inserted, `Ok(false)` if already existed.
    fn put_graph(
        &mut self,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    /// Remove an edge from the graph.
    ///
    /// Returns `Ok(true)` if removed, `Ok(false)` if not found.
    fn del_graph(
        &mut self,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    /// Add an edge to the inode graph index (INODE_GRAPH).
    ///
    /// Maintains the secondary index for efficient per-file traversal.
    /// Should be called whenever an edge is added that's part of a file.
    fn put_inode_graph(
        &mut self,
        inode: Inode,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    /// Remove an edge from the inode graph index.
    fn del_inode_graph(
        &mut self,
        inode: Inode,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> Result<bool, PristineError>;

    // ── View Operations ─────────────────────────────────────────

    /// Open or create a view.
    ///
    /// If a view with the given name exists, returns it. Otherwise creates
    /// a new shared view with zero changes and no parent.
    fn open_or_create_view(&mut self, name: &str) -> Result<ViewState, PristineError>;

    /// Create a new view with explicit scope and parent.
    ///
    /// # Errors
    ///
    /// - `ViewAlreadyExists` if a view with this name exists
    /// - `ViewNotFound` if `parent` references a non-existent view
    /// - `ViewCycleDetected` if the parent chain would create a cycle
    fn create_view(
        &mut self,
        name: &str,
        kind: ViewScope,
        parent: Option<u64>,
    ) -> Result<ViewState, PristineError>;

    /// Look up a view by its internal ID (delegates to `ViewTxnT`).
    fn get_view_by_id(&self, id: u64) -> Result<Option<ViewState>, PristineError> {
        // Default implementation delegates to ViewTxnT (which MutTxnT: ViewTxnT)
        ViewTxnT::get_view_by_id(self, id)
    }

    /// Record a change in a view.
    ///
    /// Appends the change to the view's log and updates the Merkle state.
    /// Returns the sequence number assigned to this change.
    ///
    /// # Side Effects
    ///
    /// - Updates `view.state` with the new Merkle hash
    /// - Increments `view.change_count`
    /// - Records the state in the TAGS table
    /// - Records state→sequence mapping in STATES table
    fn put_change(
        &mut self,
        view: &mut ViewState,
        change_id: NodeId,
        change_hash: &Hash,
    ) -> Result<u64, PristineError>;

    /// Remove a change from a view (unrecord).
    ///
    /// Returns `Some(seq)` if the change was found and removed, `None` if
    /// it was not in this view. Call `update_view` after to persist.
    fn del_change(
        &mut self,
        view: &mut ViewState,
        change_id: NodeId,
        change_hash: &Hash,
    ) -> Result<Option<u64>, PristineError>;

    /// Reinsert a previously unrecorded change at a specific sequence position.
    ///
    /// If `at_sequence` is beyond the current change count, appends to the end.
    /// Changes after the insertion point have their sequence numbers shifted.
    /// The view's merkle state is recomputed from scratch.
    fn reinsert_change(
        &mut self,
        view: &mut ViewState,
        change_id: NodeId,
        change_hash: &Hash,
        at_sequence: u64,
    ) -> Result<(), PristineError>;

    /// Persist the view's current state to the database.
    fn update_view(&mut self, view: &ViewState) -> Result<(), PristineError>;

    /// Delete a view from the database.
    ///
    /// Only **Draft** views can be deleted. Shared views return
    /// `CannotDeleteSharedView`. Views with children return `ViewHasChildren`.
    /// Cleans up: VIEW_CHANGES, REV_VIEW_CHANGES, STATES, TAGS.
    fn del_view(&mut self, view: &ViewState) -> Result<(), PristineError>;

    // ── Tree Operations ─────────────────────────────────────────

    /// Add a file to the tree (creates path↔inode mappings).
    fn put_tree(&mut self, path: &str, inode: Inode) -> Result<(), PristineError>;

    /// Remove a file from the tree (removes path↔inode mappings).
    fn del_tree(&mut self, path: &str) -> Result<Option<Inode>, PristineError>;

    /// Store file index entry (mtime + size + content hash) for fast status detection.
    ///
    /// Called after a file is recorded or applied. Subsequent `status()` calls
    /// can skip hashing when mtime+size match, and avoid graph reconstruction
    /// when they don't (by comparing the stored content hash instead).
    fn put_file_index(
        &mut self,
        path: &str,
        mtime_secs: i64,
        mtime_nanos: u32,
        file_size: u64,
        content_hash: &Hash,
    ) -> Result<(), PristineError>;

    /// Remove cached file index entry.
    fn del_file_index(&mut self, path: &str) -> Result<(), PristineError>;

    /// Map an inode to a graph position (creates inode↔position mappings).
    fn put_inode(&mut self, inode: Inode, pos: Position<NodeId>) -> Result<(), PristineError>;

    // ── Directory Operations ────────────────────────────────────

    /// Mark an inode as a directory with the given flags.
    ///
    /// See `directory_flags` module for flag constants
    /// (`DIR_EXPLICIT`, `DIR_EMPTY`).
    fn put_directory(&mut self, inode: Inode, flags: u8) -> Result<(), PristineError>;

    /// Remove the directory marker from an inode.
    fn del_directory(&mut self, inode: Inode) -> Result<Option<u8>, PristineError>;

    /// Update directory flags (default: delete + re-add).
    fn update_directory_flags(&mut self, inode: Inode, flags: u8) -> Result<(), PristineError> {
        self.del_directory(inode)?;
        self.put_directory(inode, flags)
    }

    /// Remove an inode mapping (removes inode↔position mappings).
    fn del_inode(&mut self, inode: Inode) -> Result<Option<Position<NodeId>>, PristineError>;

    // ── Dependency Operations ───────────────────────────────────

    /// Record that `change_id` depends on `dep_id`.
    fn put_dep(&mut self, change_id: NodeId, dep_id: NodeId) -> Result<(), PristineError>;

    /// Get all changes that the given change depends on.
    fn get_deps(&self, change_id: NodeId) -> Result<Vec<NodeId>, PristineError>;

    // ── Allocation ──────────────────────────────────────────────

    /// Allocate a new unique inode identifier.
    fn alloc_inode(&mut self) -> Result<Inode, PristineError>;

    // ── CRDT Table Operations ───────────────────────────────────

    /// Store a trunk (file) entry in the CRDT tables.
    fn put_crdt_trunk(&mut self, key: &[u8; 12], value: &[u8]) -> Result<(), PristineError>;

    /// Get a trunk entry from the CRDT tables.
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
    fn get_trunk_by_path(
        &mut self,
        path: &str,
    ) -> Result<Option<crate::crdt::TrunkId>, PristineError>;

    /// Iterate over all branches (lines) belonging to a trunk (file).
    ///
    /// Returns branch IDs in CRDT ordering (by BranchId).
    #[allow(clippy::type_complexity)]
    fn iter_trunk_branches(
        &mut self,
        trunk_key: &[u8; 12],
    ) -> Result<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>, PristineError>;

    /// Iterate over all leaves (tokens) belonging to a branch (line).
    ///
    /// Returns leaf IDs in CRDT ordering (by LeafId).
    #[allow(clippy::type_complexity)]
    fn iter_branch_leaves(
        &mut self,
        branch_key: &[u8; 12],
    ) -> Result<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>, PristineError>;

    /// Store a branch→vertex mapping for CRDT graph integration.
    ///
    /// This mapping allows finding the graph vertex when processing
    /// delete operations, which is necessary to mark edges with DELETED flags.
    fn put_crdt_branch_vertex(
        &mut self,
        branch_key: &[u8; 12],
        node_bytes: &[u8; 24],
    ) -> Result<(), PristineError>;

    /// Get the graph vertex for a branch.
    ///
    /// Returns the vertex position stored when the branch was first inserted,
    /// enabling delete operations to find and mark the corresponding graph edges.
    fn get_crdt_branch_vertex(
        &mut self,
        branch_key: &[u8; 12],
    ) -> Result<Option<crate::types::GraphNode<NodeId>>, PristineError>;

    /// Store the reverse mapping from a graph vertex to a CRDT BranchId.
    ///
    /// This is the reverse of `put_crdt_branch_vertex`, enabling efficient
    /// lookup from a graph vertex to its corresponding CRDT branch during
    /// semantic merge operations.
    fn put_crdt_vertex_branch(
        &mut self,
        vertex_key: &[u8; 24],
        branch_key: &[u8; 12],
    ) -> Result<(), PristineError>;

    /// Look up the CRDT BranchId for a graph vertex.
    ///
    /// Returns the BranchId stored by `put_crdt_vertex_branch`, enabling
    /// the semantic merge engine to find CRDT data for a graph vertex.
    fn get_crdt_vertex_branch(
        &mut self,
        vertex_key: &[u8; 24],
    ) -> Result<Option<crate::crdt::BranchId>, PristineError>;

    /// Store inode→position mapping for CRDT compatibility.
    fn put_inodes(&mut self, inode: u64, pos: &Position<NodeId>) -> Result<(), PristineError>;

    // ── Transaction Control ─────────────────────────────────────

    /// Commit the transaction, persisting all changes to the database.
    ///
    /// After commit, the transaction is consumed and cannot be used.
    fn commit(self) -> Result<(), PristineError>;

    /// Abort the transaction, discarding all changes (rollback).
    ///
    /// After abort, the transaction is consumed and cannot be used.
    fn abort(self) -> Result<(), PristineError>;
}
