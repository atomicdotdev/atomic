//! InodeGraphOps implementations for ReadTxn and WriteTxn.
//!
//! This module contains the concrete implementations of the [`InodeGraphOps`]
//! trait for both read and write transaction types, plus the edge
//! deserialization helper.

use crate::pristine::error::PristineError;
use crate::pristine::tables::{decode_inode_vertex, encode_inode_vertex, INODE_GRAPH};
use crate::pristine::txn::{ReadTxn, WriteTxn};
use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Inode, NodeId, Position, SerializedGraphEdge,
};

use redb::ReadableMultimapTable;

use super::types::{InodeAdjState, InodeGraphOps};

// EDGE DESERIALIZATION (local copy to avoid module visibility issues)

/// Deserialize bytes to a SerializedGraphEdge
#[inline]
fn deserialize_edge(bytes: &[u8; 24]) -> SerializedGraphEdge {
    let flag_and_pos = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let change_id = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let introduced_by = u64::from_le_bytes(bytes[16..24].try_into().unwrap());

    let flag = EdgeFlags::from_bits_truncate((flag_and_pos >> 56) as u8);
    let pos = flag_and_pos & ((1 << 56) - 1);

    let dest = Position::new(NodeId::new(change_id), ChangePosition::new(pos));
    SerializedGraphEdge::new(flag, dest, NodeId::new(introduced_by))
}

// ═══════════════════════════════════════════════════════════════════════
// InodeGraphOps Implementation for ReadTxn
// ═══════════════════════════════════════════════════════════════════════

impl InodeGraphOps for ReadTxn {
    type InodeError = PristineError;

    fn init_inode_adj(
        &self,
        inode: Inode,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<InodeAdjState, Self::InodeError> {
        Ok(InodeAdjState::new(inode, node, min_flag, max_flag))
    }

    fn next_inode_adj(
        &self,
        adj: &mut InodeAdjState,
    ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
        if adj.is_exhausted() {
            return None;
        }

        let table = match self.txn.open_multimap_table(INODE_GRAPH) {
            Ok(t) => t,
            Err(e) => {
                adj.mark_exhausted();
                return Some(Err(PristineError::Table(Box::new(e))));
            }
        };

        let inode_id = adj.inode.get();
        let key = encode_inode_vertex(
            inode_id,
            adj.node.change.get(),
            adj.node.start.get(),
            adj.node.end.get(),
        );

        // Get all edges for this exact span
        let values = match table.get(&key) {
            Ok(v) => v,
            Err(e) => {
                adj.mark_exhausted();
                return Some(Err(PristineError::Storage(Box::new(e))));
            }
        };

        // Collect matching edges into a vector
        let mut matching_edges: Vec<SerializedGraphEdge> = Vec::new();
        for result in values {
            match result {
                Ok(v) => {
                    let edge = deserialize_edge(v.value());
                    let flag = edge.flag();
                    if flag >= adj.min_flag && flag <= adj.max_flag {
                        matching_edges.push(edge);
                    }
                }
                Err(e) => {
                    adj.mark_exhausted();
                    return Some(Err(PristineError::Storage(Box::new(e))));
                }
            }
        }

        // Return the edge at the current position
        if adj.position < matching_edges.len() {
            let edge = matching_edges[adj.position];
            adj.advance();
            Some(Ok(edge))
        } else {
            adj.mark_exhausted();
            None
        }
    }

    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        let start_key = encode_inode_vertex(inode_id, change_id, 0, 0);
        let end_key = encode_inode_vertex(inode_id, change_id, u64::MAX, u64::MAX);

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, _values) = result?;
            let (_, v_change, v_start, v_end) = decode_inode_vertex(key.value());

            if v_change == change_id && v_start <= target_pos && target_pos < v_end {
                return Ok(Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                }));
            }
        }

        Ok(None)
    }

    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id, u64::MAX, u64::MAX, u64::MAX);

        let mut count = 0;
        let mut last_vertex: Option<(u64, u64, u64)> = None;

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, _values) = result?;
            let (_, change_id, start, end) = decode_inode_vertex(key.value());

            let current = (change_id, start, end);
            if last_vertex != Some(current) {
                count += 1;
                last_vertex = Some(current);
            }
        }

        Ok(count)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// InodeGraphOps Implementation for WriteTxn
// ═══════════════════════════════════════════════════════════════════════

impl<'a> InodeGraphOps for WriteTxn<'a> {
    type InodeError = PristineError;

    fn init_inode_adj(
        &self,
        inode: Inode,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<InodeAdjState, Self::InodeError> {
        Ok(InodeAdjState::new(inode, node, min_flag, max_flag))
    }

    fn next_inode_adj(
        &self,
        adj: &mut InodeAdjState,
    ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
        if adj.is_exhausted() {
            return None;
        }

        let table = match self.txn.open_multimap_table(INODE_GRAPH) {
            Ok(t) => t,
            Err(e) => {
                adj.mark_exhausted();
                return Some(Err(PristineError::Table(Box::new(e))));
            }
        };

        let inode_id = adj.inode.get();
        let key = encode_inode_vertex(
            inode_id,
            adj.node.change.get(),
            adj.node.start.get(),
            adj.node.end.get(),
        );

        // Get all edges for this exact span
        let values = match table.get(&key) {
            Ok(v) => v,
            Err(e) => {
                adj.mark_exhausted();
                return Some(Err(PristineError::Storage(Box::new(e))));
            }
        };

        // Collect matching edges into a vector
        let mut matching_edges: Vec<SerializedGraphEdge> = Vec::new();
        for result in values {
            match result {
                Ok(v) => {
                    let edge = deserialize_edge(v.value());
                    let flag = edge.flag();
                    if flag >= adj.min_flag && flag <= adj.max_flag {
                        matching_edges.push(edge);
                    }
                }
                Err(e) => {
                    adj.mark_exhausted();
                    return Some(Err(PristineError::Storage(Box::new(e))));
                }
            }
        }

        // Return the edge at the current position
        if adj.position < matching_edges.len() {
            let edge = matching_edges[adj.position];
            adj.advance();
            Some(Ok(edge))
        } else {
            adj.mark_exhausted();
            None
        }
    }

    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        let start_key = encode_inode_vertex(inode_id, change_id, 0, 0);
        let end_key = encode_inode_vertex(inode_id, change_id, u64::MAX, u64::MAX);

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, _values) = result?;
            let (_, v_change, v_start, v_end) = decode_inode_vertex(key.value());

            if v_change == change_id && v_start <= target_pos && target_pos < v_end {
                return Ok(Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                }));
            }
        }

        Ok(None)
    }

    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id, u64::MAX, u64::MAX, u64::MAX);

        let mut count = 0;
        let mut last_vertex: Option<(u64, u64, u64)> = None;

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, _values) = result?;
            let (_, change_id, start, end) = decode_inode_vertex(key.value());

            let current = (change_id, start, end);
            if last_vertex != Some(current) {
                count += 1;
                last_vertex = Some(current);
            }
        }

        Ok(count)
    }
}
