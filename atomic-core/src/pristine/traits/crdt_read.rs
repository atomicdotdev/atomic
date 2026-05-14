//! Read-only accessors for the CRDT tables.
//!
//! `CrdtTxnT` exposes the CRDT layer (TRUNKS, BRANCHES, LEAVES, and their
//! ordering / cross-reference tables) for read.  It's implemented by both
//! `ReadTxn` and `WriteTxn` — so any code that just needs to *inspect*
//! CRDT state can take `&impl CrdtTxnT` and accept either.
//!
//! [`MutTxnT`](super::MutTxnT) extends this trait; the write counterparts
//! (`put_crdt_*`, `del_crdt_*`) live there.

use crate::pristine::error::PristineError;

/// Read access to the CRDT tables.
///
/// All methods take `&self` because both
/// [`redb::ReadTransaction::open_table`] and
/// [`redb::WriteTransaction::open_table`] take `&self`.  This lets callers
/// hold the txn behind a shared borrow while iterating CRDT state, which is
/// the natural fit for output walkers and record-side lookups.
pub trait CrdtTxnT {
    /// Get a trunk (file) entry from the CRDT tables.
    fn get_crdt_trunk(
        &self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedTrunk>, PristineError>;

    /// Look up the CRDT trunk for an inode.
    ///
    /// Returns the raw 12-byte trunk key; decode with
    /// [`decode_trunk_id`](crate::crdt::tables::decode_trunk_id).
    fn get_crdt_inode_trunk(&self, inode: u64) -> Result<Option<[u8; 12]>, PristineError>;

    /// Get a branch (line) entry from the CRDT tables.
    fn get_crdt_branch(
        &self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedBranch>, PristineError>;

    /// Read the "after" reference for a branch.
    ///
    /// Returns `None` if the branch has no recorded after-ref (e.g., a branch
    /// inserted by an apply path that predates `BRANCH_AFTER`, or the CRDT
    /// layer was bypassed).  Returns `Some([0u8; 12])` for branches inserted
    /// at the start of the file.
    fn get_crdt_branch_after(&self, branch_key: &[u8; 12])
        -> Result<Option<[u8; 12]>, PristineError>;

    /// Get a leaf (token) entry from the CRDT tables.
    fn get_crdt_leaf(
        &self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedLeaf>, PristineError>;

    /// Look up a trunk by file path.
    fn get_trunk_by_path(
        &self,
        path: &str,
    ) -> Result<Option<crate::crdt::TrunkId>, PristineError>;

    /// Iterate over all branches (lines) belonging to a trunk (file).
    ///
    /// Returns branch IDs in CRDT ordering (by BranchId).  For file order,
    /// callers should use
    /// [`crate::crdt::queries::iter_trunk_branches_in_file_order`].
    #[allow(clippy::type_complexity)]
    fn iter_trunk_branches(
        &self,
        trunk_key: &[u8; 12],
    ) -> Result<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>, PristineError>;

    /// Iterate over all leaves (tokens) belonging to a branch (line).
    ///
    /// Returns leaf IDs in CRDT ordering (by LeafId).
    #[allow(clippy::type_complexity)]
    fn iter_branch_leaves(
        &self,
        branch_key: &[u8; 12],
    ) -> Result<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>, PristineError>;

    /// Get the graph vertex for a branch.
    ///
    /// Returns the vertex position stored when the branch was first inserted,
    /// enabling delete operations to find and mark the corresponding graph edges.
    fn get_crdt_branch_vertex(
        &self,
        branch_key: &[u8; 12],
    ) -> Result<Option<crate::types::GraphNode<crate::types::NodeId>>, PristineError>;

    /// Look up the CRDT BranchId for a graph vertex.
    ///
    /// Returns the BranchId stored by `put_crdt_vertex_branch`, enabling
    /// the semantic merge engine to find CRDT data for a graph vertex.
    fn get_crdt_vertex_branch(
        &self,
        vertex_key: &[u8; 24],
    ) -> Result<Option<crate::crdt::BranchId>, PristineError>;
}
