//! Atomic replacement of native derived pristine indexes.

use crate::pristine::error::PristineError;
use crate::pristine::path_claim::PathClaimEntry;
use crate::types::{Inode, NodeId, Position};

use super::view::StoredConflict;

/// Complete replacement payload for pristine indexes derived from authoritative
/// graph and view history.
///
/// Row order is not significant. Implementations normalize rows before
/// validation and storage so equivalent payloads produce deterministic tables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeDerivedIndexes {
    /// All additive PATH_CLAIMS rows.
    pub path_claims: Vec<PathClaimEntry>,
    /// Complete TREE rows (`path -> inode`).
    pub tree: Vec<(String, Inode)>,
    /// Complete REV_TREE rows (`inode -> path`).
    pub rev_tree: Vec<(Inode, String)>,
    /// Complete INODES rows (`inode -> graph root position`).
    pub inodes: Vec<(Inode, Position<NodeId>)>,
    /// Complete REV_INODES rows (`graph root position -> inode`).
    pub rev_inodes: Vec<(Position<NodeId>, Inode)>,
    /// Complete DIRECTORIES rows (`inode -> directory flags`).
    pub directories: Vec<(Inode, u8)>,
    /// Complete CONFLICTS rows (`view id, inode, conflict records`).
    pub conflicts: Vec<(u64, Inode, Vec<StoredConflict>)>,
}

/// Destructive replacement access for native indexes that can be reconstructed
/// from authoritative graph and view history.
pub trait NativeDerivedIndexesMutTxnT {
    /// Validate, clear, and replace all native derived indexes in this write
    /// transaction.
    ///
    /// Validation completes before any table is changed. The graph,
    /// inode-scoped graph, change identities, view logs, CRDT state, file index,
    /// and working copy are not modified.
    fn replace_native_derived_indexes(
        &mut self,
        replacement: &NativeDerivedIndexes,
    ) -> Result<(), PristineError>;
}
