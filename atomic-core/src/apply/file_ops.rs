//! Apply FileOps (semantic layer) to CRDT tables.
//!
//! This module provides functions to populate the CRDT tables (TRUNKS, BRANCHES,
//! LEAVES) from the semantic layer operations stored in a change's `file_ops`.
//!
//! # Overview
//!
//! When a change is applied, we need to populate two layers:
//!
//! 1. **Graph layer** (hunks/atoms): Vertices and edges in the pristine graph
//! 2. **Semantic layer** (file_ops): CRDT tables for human-readable operations
//!
//! This module handles the semantic layer. The graph layer is handled by
//! the existing `write_new_vertex` and `write_edge_map` functions.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                    Apply FileOps to CRDT Tables                         │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Change.file_ops                     CRDT Tables                        │
//! │  ┌──────────────────────┐           ┌─────────────────────────────┐    │
//! │  │ Vec<FileOps>         │  apply    │ TRUNKS (file metadata)      │    │
//! │  │   └── TrunkOp        │ ────────► │ BRANCHES (line metadata)    │    │
//! │  │   └── Vec<LineOps>   │           │ LEAVES (token metadata)     │    │
//! │  │       └── BranchOp   │           │ PATH_TRUNK (path index)     │    │
//! │  │       └── Vec<LeafOp>│           │ TRUNK_BRANCHES (ordering)   │    │
//! │  └──────────────────────┘           │ BRANCH_LEAVES (ordering)    │    │
//! │                                      └─────────────────────────────┘    │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Why Both Layers?
//!
//! - **Graph layer**: Storage, content-addressing, merging at byte level
//! - **Semantic layer**: Human-readable diffs, token-level blame, code review
//!
//! Both are required. The graph stores efficiently; CRDT makes it understandable.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::apply::file_ops::{apply_file_ops, ApplyFileOpsStats};
//! use atomic_core::change::Change;
//!
//! // After applying hunks to the graph, apply file_ops to CRDT tables
//! if change.has_file_ops() {
//!     let stats = apply_file_ops(&mut txn, change_id, change.file_ops())?;
//!     println!("Applied {} trunks, {} branches, {} leaves",
//!         stats.trunks_created, stats.branches_created, stats.leaves_created);
//! }
//! ```

use crate::change::{Encoding, FileOps, LineOps};
use crate::crdt::tables::{
    encode_branch_id, encode_branch_value, encode_leaf_id, encode_leaf_value, encode_trunk_id,
    encode_trunk_value, encode_vertex_position, SerializedBranch, SerializedLeaf, SerializedTrunk,
};
use crate::crdt::{
    BranchId, BranchOp, BranchState, LeafId, LeafOp, LeafState, TrunkId, TrunkOp, TrunkState,
};
use crate::pristine::{MutTxnT, PristineError, PristineResult};
use crate::types::{GraphNode, NodeId};

// Statistics

/// Statistics from applying FileOps to CRDT tables.
#[derive(Debug, Clone, Default)]
pub struct ApplyFileOpsStats {
    /// Number of trunks (files) created.
    pub trunks_created: usize,
    /// Number of trunks deleted.
    pub trunks_deleted: usize,
    /// Number of trunks moved.
    pub trunks_moved: usize,
    /// Number of branches (lines) created.
    pub branches_created: usize,
    /// Number of branches deleted.
    pub branches_deleted: usize,
    /// Number of branches restored.
    pub branches_restored: usize,
    /// Number of branches re-parented (moved to a new chain position).
    pub branches_reparented: usize,
    /// Number of leaves (tokens) created.
    pub leaves_created: usize,
    /// Number of leaves deleted.
    pub leaves_deleted: usize,
    /// Number of leaves replaced.
    pub leaves_replaced: usize,
    /// Number of leaves restored.
    pub leaves_restored: usize,
}

impl ApplyFileOpsStats {
    /// Creates new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns total trunk operations.
    pub fn total_trunk_ops(&self) -> usize {
        self.trunks_created + self.trunks_deleted + self.trunks_moved
    }

    /// Returns total branch operations.
    pub fn total_branch_ops(&self) -> usize {
        self.branches_created
            + self.branches_deleted
            + self.branches_restored
            + self.branches_reparented
    }

    /// Returns total leaf operations.
    pub fn total_leaf_ops(&self) -> usize {
        self.leaves_created + self.leaves_deleted + self.leaves_replaced + self.leaves_restored
    }

    /// Returns true if any operations were applied.
    pub fn has_operations(&self) -> bool {
        self.total_trunk_ops() > 0 || self.total_branch_ops() > 0 || self.total_leaf_ops() > 0
    }
}

// Apply Functions

/// Apply all FileOps from a change to the CRDT tables.
///
/// This populates TRUNKS, BRANCHES, and LEAVES tables with the semantic
/// layer data, enabling human-readable diffs and token-level blame.
///
/// # Arguments
///
/// * `txn` - Mutable transaction for database writes
/// * `change_id` - The NodeId of the change being applied
/// * `file_ops` - The FileOps to apply
///
/// # Returns
///
/// Statistics about what was applied.
///
/// # Example
///
/// ```rust,ignore
/// let stats = apply_file_ops(&mut txn, change_id, change.file_ops())?;
/// ```
pub fn apply_file_ops<T: MutTxnT>(
    txn: &mut T,
    change_id: NodeId,
    file_ops: &[FileOps],
) -> PristineResult<ApplyFileOpsStats> {
    let mut stats = ApplyFileOpsStats::new();
    for ops in file_ops {
        apply_single_file_ops(txn, change_id, ops, &mut stats)?;
    }
    Ok(stats)
}

/// Apply FileOps for a single file.
fn apply_single_file_ops<T: MutTxnT>(
    txn: &mut T,
    change_id: NodeId,
    ops: &FileOps,
    stats: &mut ApplyFileOpsStats,
) -> PristineResult<()> {
    let trunk_id = ops.trunk_id();
    let path = ops.path();

    // Apply trunk operation if present
    if let Some(trunk_op) = ops.trunk_op() {
        apply_trunk_op(txn, change_id, trunk_id, path, trunk_op, stats)?;
    }

    // Apply line operations
    for line_ops in ops.line_ops() {
        apply_line_ops_with_position(txn, change_id, trunk_id, line_ops, stats)?;
    }

    Ok(())
}

/// Apply a TrunkOp (file-level operation).
fn apply_trunk_op<T: MutTxnT>(
    txn: &mut T,
    _change_id: NodeId,
    trunk_id: TrunkId,
    path: &str,
    trunk_op: &TrunkOp,
    stats: &mut ApplyFileOpsStats,
) -> PristineResult<()> {
    match trunk_op {
        TrunkOp::Create { encoding, .. } => {
            // Use the inode the tree layer has already allocated for this
            // path during `write_recorded`'s FileAdd pre-pass.  Allocating
            // a fresh one here would create a parallel inode for the same
            // file: the tree layer would index `path → tree_inode → pos`
            // while the CRDT layer would index `crdt_inode → trunk`, and
            // a caller asking "what's the CRDT trunk for the inode the
            // tree returned for this path" would get nothing — even
            // though both rows exist.
            //
            // Falling back to `alloc_inode` only when the tree has no
            // entry preserves the original behaviour for CRDT-only
            // callers (no FileAdd hunk).
            let inode = match txn.get_inode(path)? {
                Some(i) => i,
                None => txn.alloc_inode()?,
            };

            // Create the serialized trunk record
            let serialized = SerializedTrunk {
                inode,
                state: TrunkState::Alive,
                encoding: encoding_to_u8(encoding.as_ref()),
                path: path.to_string(),
            };

            // Store in CRDT tables
            put_trunk(txn, trunk_id, &serialized)?;
            stats.trunks_created += 1;
        }

        TrunkOp::Delete { .. } => {
            // Mark trunk as deleted (don't remove - CRDT semantics)
            update_trunk_state(txn, trunk_id, TrunkState::Deleted)?;
            stats.trunks_deleted += 1;
        }

        TrunkOp::Move { new_path, .. } => {
            // Update trunk path
            update_trunk_path(txn, trunk_id, new_path)?;
            stats.trunks_moved += 1;
        }

        TrunkOp::Undelete { .. } => {
            // Restore trunk to alive state
            update_trunk_state(txn, trunk_id, TrunkState::Alive)?;
            // Count as created for stats purposes
            stats.trunks_created += 1;
        }
    }

    Ok(())
}

/// Apply a BranchOp (line-level operation) with optional graph position linkage.
///
/// This function applies the CRDT line operation and, if the LineOps has been
/// enriched with a content_range during globalization, also populates the
/// BRANCH_VERTEX table to link the CRDT branch to its graph vertex.
fn apply_line_ops_with_position<T: MutTxnT>(
    txn: &mut T,
    change_id: NodeId,
    trunk_id: TrunkId,
    line_ops: &LineOps,
    stats: &mut ApplyFileOpsStats,
) -> PristineResult<()> {
    // Resolve the BranchId.  The change file stores BranchIds with
    // `change_id = NodeId::ROOT` as a placeholder meaning "this change
    // — fill in the real id at apply time" (analogous to how vertex
    // positions store `change: None` and resolve to the current change
    // during apply).  Without this resolution, every commit's `Insert`
    // ops carry `BranchId(ROOT, 0)`, `BranchId(ROOT, 1)`, … which
    // collide across changes in the BRANCHES table, silently
    // overwriting each other's rows.
    //
    // We treat `BranchId::ROOT` (i.e. change_id == NodeId::ROOT) as
    // the placeholder marker.  Branches that genuinely reference a
    // prior change (e.g. an Insert with `after: Some(existing_branch)`)
    // already carry that change's real id.
    let raw_branch_id = line_ops.branch_id();
    let branch_id = if raw_branch_id.change_id().is_root() {
        BranchId::new(change_id, raw_branch_id.branch_idx())
    } else {
        raw_branch_id
    };
    let branch_op = line_ops.operation();

    match branch_op {
        BranchOp::Insert { after, content } => {
            // Create serialized branch record
            let serialized = SerializedBranch {
                trunk_id,
                state: BranchState::Alive,
                line_hash: 0, // Will be computed if needed
            };

            put_branch(txn, trunk_id, branch_id, &serialized)?;
            stats.branches_created += 1;

            // Record the after-reference so file order can be reconstructed.
            //
            // Like `branch_id` itself (see the substitution at the top of
            // this function), an `after` ref with `change_id == ROOT` is a
            // *placeholder* meaning "another branch from this same change"
            // — emitted by the recorder before the apply-time change_id is
            // known.  We must substitute the real change_id here, not
            // collapse to the file-start sentinel, otherwise every branch
            // from a single FileAdd would end up listed as a sibling of
            // the file's first line and the file-order walk would emit
            // them in BranchId-sort order rather than insertion order.
            //
            // `None` means genuine "start of file" and stays as the
            // all-zeros sentinel.
            let after_key = match after {
                Some(a) if a.change_id().is_root() => {
                    encode_branch_id(&BranchId::new(change_id, a.branch_idx()))
                }
                Some(a) => encode_branch_id(a),
                None => [0u8; 12],
            };
            txn.put_crdt_branch_after(&encode_branch_id(&branch_id), &after_key)?;

            // If we have enriched position info, link to the graph vertex
            if let Some((start, end)) = line_ops.content_range() {
                // Create the GraphNode that corresponds to this branch
                let graph_node = GraphNode {
                    change: change_id,
                    start,
                    end,
                };

                // Store the mapping in BRANCH_VERTEX
                let branch_key = encode_branch_id(&branch_id);
                let vertex_bytes = encode_vertex_position(&graph_node);
                txn.put_crdt_branch_vertex(&branch_key, &vertex_bytes)?;

                // Store the Graph→CRDT reverse mapping (for semantic merge)
                txn.put_crdt_vertex_branch(&vertex_bytes, &branch_key)?;
            }

            // Apply leaf operations for this line's tokens
            for (leaf_idx, leaf_op) in content.iter().enumerate() {
                let leaf_id = LeafId::new(branch_id.change_id(), leaf_idx as u32);
                apply_leaf_op(txn, branch_id, leaf_id, leaf_op, stats)?;
            }
        }

        BranchOp::Delete { .. } => {
            update_branch_state(txn, branch_id, BranchState::Deleted)?;
            stats.branches_deleted += 1;
        }

        BranchOp::Modify { new_content, .. } => {
            // A Modify is semantically a delete-then-insert at the storage
            // layer: mark the old branch deleted, create a new one with the
            // new content.  The old_content is carried only for diff display
            // and is not persisted separately.
            update_branch_state(txn, branch_id, BranchState::Deleted)?;
            stats.branches_deleted += 1;

            // Create the replacement branch (reuse branch_id — the CRDT
            // model allows this because the Modify preserves line identity)
            let serialized = SerializedBranch {
                trunk_id,
                state: BranchState::Alive,
                line_hash: 0,
            };
            put_branch(txn, trunk_id, branch_id, &serialized)?;
            stats.branches_created += 1;

            // Re-point the CRDT branch at the *new* graph vertex.  Globalize
            // sets `content_range` to the modifying change's content blob
            // range for Modify ops the same way it does for Insert (see
            // `enrich_file_ops_for_edit` in record/workflow/globalize/
            // pipeline.rs).  Without this update, BRANCH_VERTEX would still
            // reference the pre-Modify vertex — and the CRDT-driven output
            // walker would fetch stale bytes.
            //
            // **Required invariant:** every Modify must arrive with
            // `content_range` set.  If it doesn't, globalize's enrich pass
            // failed to match this LineOps by `new_line_num` — typically
            // because the LineOps was emitted without a `new_line_num` or
            // because the surrounding hunk's range doesn't cover this line.
            // Either way, the branch's BRANCH_VERTEX would silently keep
            // pointing at the pre-Modify content, producing a stale-bytes
            // bug that compounds over commit sequences.
            //
            // We hard-fail rather than silently mis-record.  This caught
            // the hyperfine extended-sequence drift at commit 5 — Modifies
            // were arriving without content_range and the walker was
            // emitting stale bytes from earlier changes.
            match line_ops.content_range() {
                Some((start, end)) => {
                    let graph_node = GraphNode {
                        change: change_id,
                        start,
                        end,
                    };
                    let branch_key = encode_branch_id(&branch_id);
                    let vertex_bytes = encode_vertex_position(&graph_node);
                    txn.put_crdt_branch_vertex(&branch_key, &vertex_bytes)?;
                    txn.put_crdt_vertex_branch(&vertex_bytes, &branch_key)?;
                }
                None => {
                    return Err(PristineError::Inconsistent {
                        message: format!(
                            "apply_line_ops_with_position: Modify of branch {:?} \
                             arrived without content_range — globalize enrich \
                             failed for change_id={:?} trunk_id={:?}.  Without \
                             a new content_range the CRDT branch would silently \
                             keep pointing at its pre-Modify graph vertex, \
                             producing stale-bytes drift across commits.",
                            branch_id, change_id, trunk_id
                        ),
                    });
                }
            }

            // Apply leaf operations for the new content
            for (leaf_idx, leaf_op) in new_content.iter().enumerate() {
                let leaf_id = LeafId::new(branch_id.change_id(), leaf_idx as u32);
                apply_leaf_op(txn, branch_id, leaf_id, leaf_op, stats)?;
            }
        }

        BranchOp::Restore { .. } => {
            update_branch_state(txn, branch_id, BranchState::Alive)?;
            stats.branches_restored += 1;
        }

        BranchOp::Reparent { new_after, .. } => {
            // Position-only change: rewrite BRANCH_AFTER, leave state and
            // content alone.  The walker reads BRANCH_AFTER directly, so
            // this is the entire effect.
            //
            // Same placeholder substitution rule as `BranchOp::Insert.after`
            // (top of the Insert arm above): a placeholder ref with
            // `change_id == ROOT` resolves to the current change.  `None`
            // is the all-zeros sentinel for "start of file".
            let new_after_key = match new_after {
                Some(a) if a.change_id().is_root() => {
                    encode_branch_id(&BranchId::new(change_id, a.branch_idx()))
                }
                Some(a) => encode_branch_id(a),
                None => [0u8; 12],
            };
            txn.put_crdt_branch_after(&encode_branch_id(&branch_id), &new_after_key)?;
            stats.branches_reparented += 1;
        }
    }

    Ok(())
}

/// Apply a LeafOp (token-level operation).
fn apply_leaf_op<T: MutTxnT>(
    txn: &mut T,
    branch_id: BranchId,
    leaf_id: LeafId,
    leaf_op: &LeafOp,
    stats: &mut ApplyFileOpsStats,
) -> PristineResult<()> {
    match leaf_op {
        LeafOp::Insert { kind, content, .. } => {
            // Create serialized leaf record
            // Note: content_start/end would be set during content blob assembly
            let serialized = SerializedLeaf {
                branch_id,
                kind: *kind,
                state: LeafState::Alive,
                content_start: 0,
                content_end: content.len() as u32,
            };

            put_leaf(txn, branch_id, leaf_id, &serialized)?;
            stats.leaves_created += 1;
        }

        LeafOp::Delete { .. } => {
            update_leaf_state(txn, leaf_id, LeafState::Deleted)?;
            stats.leaves_deleted += 1;
        }

        LeafOp::Replace { new_content, .. } => {
            // Replace keeps the same leaf ID but changes content
            update_leaf_content(txn, leaf_id, new_content)?;
            stats.leaves_replaced += 1;
        }

        LeafOp::Restore { .. } => {
            update_leaf_state(txn, leaf_id, LeafState::Alive)?;
            stats.leaves_restored += 1;
        }
    }

    Ok(())
}

/// Convert Encoding to u8 for storage.
fn encoding_to_u8(encoding: Option<&Encoding>) -> u8 {
    match encoding {
        None => 0,
        Some(Encoding::Utf8) => 1,
        Some(Encoding::Utf16Le) => 2,
        Some(Encoding::Utf16Be) => 3,
        Some(Encoding::Binary) => 4,
        Some(Encoding::Latin1) => 5,
    }
}

// Low-Level Table Operations

/// Store a trunk in the CRDT tables.
fn put_trunk<T: MutTxnT>(
    txn: &mut T,
    trunk_id: TrunkId,
    serialized: &SerializedTrunk,
) -> PristineResult<()> {
    let key = encode_trunk_id(&trunk_id);
    let value = encode_trunk_value(serialized);

    txn.put_crdt_trunk(&key, &value)?;

    // Also update path index
    txn.put_crdt_path_trunk(&serialized.path, &key)?;

    // And inode index
    txn.put_crdt_inode_trunk(serialized.inode.get(), &key)?;

    Ok(())
}

/// Update a trunk's state.
fn update_trunk_state<T: MutTxnT>(
    txn: &mut T,
    trunk_id: TrunkId,
    state: TrunkState,
) -> PristineResult<()> {
    let key = encode_trunk_id(&trunk_id);

    // Get existing trunk
    if let Some(mut serialized) = txn.get_crdt_trunk(&key)? {
        // Update state in the serialized data
        serialized.state = state;

        // Re-encode and store
        let value = encode_trunk_value(&serialized);
        txn.put_crdt_trunk(&key, &value)?;
    }

    Ok(())
}

/// Update a trunk's path.
fn update_trunk_path<T: MutTxnT>(
    txn: &mut T,
    trunk_id: TrunkId,
    new_path: &str,
) -> PristineResult<()> {
    let key = encode_trunk_id(&trunk_id);

    // Get existing trunk
    if let Some(mut serialized) = txn.get_crdt_trunk(&key)? {
        // Remove old path index
        txn.del_crdt_path_trunk(&serialized.path)?;

        // Update path
        serialized.path = new_path.to_string();

        // Re-encode and store
        let value = encode_trunk_value(&serialized);
        txn.put_crdt_trunk(&key, &value)?;

        // Add new path index
        txn.put_crdt_path_trunk(new_path, &key)?;
    }

    Ok(())
}

/// Store a branch in the CRDT tables.
fn put_branch<T: MutTxnT>(
    txn: &mut T,
    trunk_id: TrunkId,
    branch_id: BranchId,
    serialized: &SerializedBranch,
) -> PristineResult<()> {
    let key = encode_branch_id(&branch_id);
    let value = encode_branch_value(serialized);

    txn.put_crdt_branch(&key, &value)?;

    // Update trunk->branch ordering
    let trunk_key = encode_trunk_id(&trunk_id);
    txn.put_crdt_trunk_branch(&trunk_key, &key)?;

    Ok(())
}

/// Update a branch's state.
fn update_branch_state<T: MutTxnT>(
    txn: &mut T,
    branch_id: BranchId,
    state: BranchState,
) -> PristineResult<()> {
    let key = encode_branch_id(&branch_id);

    // Get existing branch
    if let Some(mut serialized) = txn.get_crdt_branch(&key)? {
        // Update state
        serialized.state = state;

        // Re-encode and store
        let value = encode_branch_value(&serialized);
        txn.put_crdt_branch(&key, &value)?;
    }

    Ok(())
}

/// Store a leaf in the CRDT tables.
fn put_leaf<T: MutTxnT>(
    txn: &mut T,
    branch_id: BranchId,
    leaf_id: LeafId,
    serialized: &SerializedLeaf,
) -> PristineResult<()> {
    let key = encode_leaf_id(&leaf_id);
    let value = encode_leaf_value(serialized);

    txn.put_crdt_leaf(&key, &value)?;

    // Update branch->leaf ordering
    let branch_key = encode_branch_id(&branch_id);
    txn.put_crdt_branch_leaf(&branch_key, &key)?;

    Ok(())
}

/// Update a leaf's state.
fn update_leaf_state<T: MutTxnT>(
    txn: &mut T,
    leaf_id: LeafId,
    state: LeafState,
) -> PristineResult<()> {
    let key = encode_leaf_id(&leaf_id);

    // Get existing leaf
    if let Some(mut serialized) = txn.get_crdt_leaf(&key)? {
        // Update state
        serialized.state = state;

        // Re-encode and store
        let value = encode_leaf_value(&serialized);
        txn.put_crdt_leaf(&key, &value)?;
    }

    Ok(())
}

/// Update a leaf's content.
fn update_leaf_content<T: MutTxnT>(
    txn: &mut T,
    leaf_id: LeafId,
    new_content: &[u8],
) -> PristineResult<()> {
    let key = encode_leaf_id(&leaf_id);

    // Get existing leaf
    if let Some(mut serialized) = txn.get_crdt_leaf(&key)? {
        // Update content range (actual content is in the change's content blob)
        serialized.content_end = serialized.content_start + new_content.len() as u32;

        // Re-encode and store
        let value = encode_leaf_value(&serialized);
        txn.put_crdt_leaf(&key, &value)?;
    }

    Ok(())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_new() {
        let stats = ApplyFileOpsStats::new();
        assert_eq!(stats.trunks_created, 0);
        assert_eq!(stats.branches_created, 0);
        assert_eq!(stats.leaves_created, 0);
        assert!(!stats.has_operations());
    }

    #[test]
    fn test_stats_total_trunk_ops() {
        let mut stats = ApplyFileOpsStats::new();
        stats.trunks_created = 2;
        stats.trunks_deleted = 1;
        stats.trunks_moved = 1;
        assert_eq!(stats.total_trunk_ops(), 4);
    }

    #[test]
    fn test_stats_total_branch_ops() {
        let mut stats = ApplyFileOpsStats::new();
        stats.branches_created = 5;
        stats.branches_deleted = 2;
        stats.branches_restored = 1;
        assert_eq!(stats.total_branch_ops(), 8);
    }

    #[test]
    fn test_stats_total_leaf_ops() {
        let mut stats = ApplyFileOpsStats::new();
        stats.leaves_created = 10;
        stats.leaves_deleted = 3;
        stats.leaves_replaced = 2;
        stats.leaves_restored = 1;
        assert_eq!(stats.total_leaf_ops(), 16);
    }

    #[test]
    fn test_stats_has_operations() {
        let mut stats = ApplyFileOpsStats::new();
        assert!(!stats.has_operations());

        stats.trunks_created = 1;
        assert!(stats.has_operations());
    }

    #[test]
    fn test_stats_default() {
        let stats = ApplyFileOpsStats::default();
        assert_eq!(stats.total_trunk_ops(), 0);
        assert_eq!(stats.total_branch_ops(), 0);
        assert_eq!(stats.total_leaf_ops(), 0);
    }
}
