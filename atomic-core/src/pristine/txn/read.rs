//! Read-only transaction implementation
//!
//! This module provides the `ReadTxn` struct which implements read-only
//! access to the pristine database.

use redb::{ReadTransaction, ReadableTable};

use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Merkle, NodeId, Position,
    SerializedGraphEdge,
};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::*;
use crate::pristine::traits::{GraphTxnT, StackState, StackTxnT, TreeTxnT};

use super::helpers::{deserialize_edge, deserialize_stack_state, AdjIterator};

/// Read-only transaction
///
/// Provides read access to the pristine database. Multiple read transactions
/// can be active simultaneously.
pub struct ReadTxn {
    pub(crate) txn: ReadTransaction,
}

impl ReadTxn {
    /// Create a new read transaction
    pub(crate) fn new(txn: ReadTransaction) -> Self {
        Self { txn }
    }
}

// =============================================================================
// GraphTxnT Implementation
// =============================================================================

impl GraphTxnT for ReadTxn {
    type Adj = AdjIterator;

    fn get_external(&self, id: NodeId) -> PristineResult<Option<Hash>> {
        let table = self.txn.open_table(EXTERNAL)?;
        let result = table.get(id.get())?;
        match result {
            Some(value) => {
                let bytes: &[u8; 32] = value.value();
                Ok(Some(Hash::from_bytes(*bytes)))
            }
            None => Ok(None),
        }
    }

    fn get_internal(&self, hash: &Hash) -> PristineResult<Option<NodeId>> {
        let table = self.txn.open_table(INTERNAL)?;
        let result = table.get(hash.as_bytes())?;
        match result {
            Some(value) => Ok(Some(NodeId::new(value.value()))),
            None => Ok(None),
        }
    }

    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> PristineResult<Self::Adj> {
        let table = self.txn.open_multimap_table(GRAPH)?;
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());

        let mut edges = Vec::new();
        for result in table.get(&key)? {
            if let Ok(v) = result {
                let bytes: &[u8; 24] = v.value();
                let edge = deserialize_edge(bytes);
                let flag = edge.flag();
                if flag >= min_flag && flag <= max_flag {
                    edges.push(edge);
                }
            }
        }

        Ok(AdjIterator::new(edges))
    }

    fn find_block(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        // Handle ROOT position specially - ROOT is a virtual span that doesn't
        // exist in the database. It represents the repository root and is the
        // parent of all top-level files and directories.
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        let table = self.txn.open_multimap_table(GRAPH)?;

        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        let start_key = encode_vertex(change_id, 0, 0);
        let end_key = encode_vertex(change_id + 1, 0, 0);

        // Track empty span match as fallback
        let mut empty_vertex_match: Option<GraphNode<NodeId>> = None;

        for result in table.range::<&[u8; 24]>(&start_key..&end_key)? {
            let (key, _values) = result?;
            let (v_change, v_start, v_end) = decode_vertex(key.value());

            if v_change != change_id {
                continue;
            }

            // Check for non-empty span containing this position.
            // For edges pointing to content, we want to find the content span
            // even if there's an empty inode span at the same start position.
            // This is critical for graph traversal: an edge to position 9 should
            // find content span V[9:23], not inode span V[9:9].
            if v_start != v_end && v_start <= target_pos && target_pos < v_end {
                return Ok(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }

            // Track empty span at exact position as fallback
            if v_start == v_end && v_start == target_pos && empty_vertex_match.is_none() {
                empty_vertex_match = Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
        }

        // Return empty span if no non-empty span matched
        if let Some(found) = empty_vertex_match {
            return Ok(found);
        }

        Err(PristineError::BlockNotFound {
            change: change_id,
            pos: target_pos,
        })
    }

    fn find_block_end(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        // Handle ROOT position specially
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        let table = self.txn.open_multimap_table(GRAPH)?;

        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        // FIRST: Check for empty span at exact position using direct lookup.
        // This is important because empty vertices like inode markers (e.g., V[9:9])
        // must be found when predecessors references position 9, even if there's
        // another span like V[0:9] that also ends at position 9.
        // Without this direct lookup, iteration would return V[0:9] first since
        // it has a lower start position.
        let empty_key = encode_vertex(change_id, target_pos, target_pos);
        if table.get(&empty_key)?.next().is_some() {
            return Ok(GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(target_pos),
                end: ChangePosition::new(target_pos),
            });
        }

        // SECOND: Fall back to iteration to find vertices that end at this position
        let start_key = encode_vertex(change_id, 0, 0);
        let end_key = encode_vertex(change_id + 1, 0, 0);

        // Look for a span that ends at this position or contains it
        for result in table.range::<&[u8; 24]>(&start_key..&end_key)? {
            let (key, _values) = result?;
            let (v_change, v_start, v_end) = decode_vertex(key.value());

            if v_change != change_id {
                continue;
            }

            // Check for span that ends at this position
            if v_end == target_pos && v_start < v_end {
                return Ok(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }

            // Also check if position falls within [start, end)
            if v_start <= target_pos && target_pos < v_end {
                return Ok(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
        }

        Err(PristineError::BlockNotFound {
            change: change_id,
            pos: target_pos,
        })
    }

    fn has_vertex(&self, node: GraphNode<NodeId>) -> PristineResult<bool> {
        let table = self.txn.open_multimap_table(GRAPH)?;
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        let has = table.get(&key)?.next().is_some();
        Ok(has)
    }

    fn get_node_type(&self, node_id: NodeId) -> PristineResult<Option<u8>> {
        let table = self.txn.open_table(NODE_TYPES)?;
        let result = table.get(node_id.get())?;
        Ok(result.map(|v| v.value()))
    }

    fn get_rev_deps(&self, dep_id: NodeId) -> PristineResult<Vec<NodeId>> {
        let table = self.txn.open_multimap_table(REV_DEPS)?;
        let mut result = Vec::new();
        let iter = table.get(dep_id.get())?;
        for item in iter {
            let value = item?;
            result.push(NodeId::new(value.value()));
        }
        Ok(result)
    }
}

// =============================================================================
// StackTxnT Implementation
// =============================================================================

impl StackTxnT for ReadTxn {
    fn get_stack(&self, name: &str) -> PristineResult<Option<StackState>> {
        let table = self.txn.open_table(STACKS)?;
        let result = table.get(name)?;
        match result {
            Some(value) => {
                let bytes = value.value();
                let state = deserialize_stack_state(bytes)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    fn list_stacks(&self) -> PristineResult<Vec<String>> {
        let table = self.txn.open_table(STACKS)?;
        let mut names = Vec::new();
        for result in table.iter()? {
            if let Ok((k, _)) = result {
                names.push(k.value().to_string());
            }
        }
        Ok(names)
    }

    fn get_change_seq(&self, stack: &StackState, change_id: NodeId) -> PristineResult<Option<u64>> {
        let table = self.txn.open_table(REV_STACK_CHANGES)?;
        let key = encode_stack_seq(stack.id, change_id.get());
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(value.value())),
            None => Ok(None),
        }
    }

    fn get_change_at_seq(&self, stack: &StackState, seq: u64) -> PristineResult<Option<NodeId>> {
        let table = self.txn.open_table(STACK_CHANGES)?;
        let key = encode_stack_seq(stack.id, seq);
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(NodeId::new(value.value()))),
            None => Ok(None),
        }
    }

    fn iter_changes(
        &self,
        stack: &StackState,
        from_seq: u64,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>>
    {
        let changes_table = self.txn.open_table(STACK_CHANGES)?;
        let tags_table = self.txn.open_table(TAGS)?;

        let stack_id = stack.id;
        let start_key = encode_stack_seq(stack_id, from_seq);
        let end_key = encode_stack_seq(stack_id + 1, 0);

        // Collect into a Vec to avoid lifetime issues
        let mut results = Vec::new();
        for result in changes_table.range::<&[u8; 16]>(&start_key..&end_key)? {
            match result {
                Ok((key, value)) => {
                    let (_, seq) = decode_stack_seq(key.value());
                    let change_id = NodeId::new(value.value());

                    let tag_key = encode_stack_seq(stack_id, seq);
                    let merkle = match tags_table.get(&tag_key) {
                        Ok(Some(m)) => Merkle::from_bytes(*m.value()),
                        _ => Merkle::ZERO,
                    };

                    results.push(Ok((seq, change_id, merkle)));
                }
                Err(e) => {
                    results.push(Err(PristineError::Storage(e)));
                }
            }
        }

        Ok(Box::new(results.into_iter()))
    }
}

// =============================================================================
// TreeTxnT Implementation
// =============================================================================

impl TreeTxnT for ReadTxn {
    fn get_inode(&self, path: &str) -> PristineResult<Option<Inode>> {
        let table = self.txn.open_table(TREE)?;
        let result = table.get(path)?;
        match result {
            Some(value) => Ok(Some(Inode::new(value.value()))),
            None => Ok(None),
        }
    }

    fn get_directory_flags(&self, inode: Inode) -> PristineResult<Option<u8>> {
        let table = self.txn.open_table(DIRECTORIES)?;
        let result = table.get(inode.get())?;
        Ok(result.map(|v| v.value()))
    }

    fn get_path(&self, inode: Inode) -> PristineResult<Option<String>> {
        let table = self.txn.open_table(REV_TREE)?;
        let result = table.get(inode.get())?;
        match result {
            Some(value) => Ok(Some(value.value().to_string())),
            None => Ok(None),
        }
    }

    fn inode_position(&self, inode: Inode) -> PristineResult<Option<Position<NodeId>>> {
        let table = self.txn.open_table(INODES)?;
        let result = table.get(inode.get())?;
        match result {
            Some(value) => {
                let (change_id, pos) = decode_position(value.value());
                Ok(Some(Position::new(
                    NodeId::new(change_id),
                    ChangePosition::new(pos),
                )))
            }
            None => Ok(None),
        }
    }

    fn position_inode(&self, pos: Position<NodeId>) -> PristineResult<Option<Inode>> {
        let table = self.txn.open_table(REV_INODES)?;
        let key = encode_position(pos.change.get(), pos.pos.get());
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(Inode::new(value.value()))),
            None => Ok(None),
        }
    }

    fn iter_tree(
        &self,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>> {
        let table = self.txn.open_table(TREE)?;
        // Collect to avoid lifetime issues
        let mut results = Vec::new();
        for result in table.iter()? {
            match result {
                Ok((k, v)) => {
                    results.push(Ok((k.value().to_string(), Inode::new(v.value()))));
                }
                Err(e) => {
                    results.push(Err(PristineError::Storage(e)));
                }
            }
        }
        Ok(Box::new(results.into_iter()))
    }

    fn iter_inode_vertices(
        &self,
        inode: Inode,
    ) -> PristineResult<
        Box<
            dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                + '_,
        >,
    > {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id + 1, 0, 0, 0);

        // Collect to avoid lifetime issues
        let mut results = Vec::new();
        for result in table.range::<&[u8; 32]>(&start_key..&end_key)? {
            match result {
                Ok((key, values)) => {
                    let (_, change_id, start, end) = decode_inode_vertex(key.value());
                    let node = GraphNode {
                        change: NodeId::new(change_id),
                        start: ChangePosition::new(start),
                        end: ChangePosition::new(end),
                    };

                    for v in values {
                        if let Ok(v) = v {
                            let edge = deserialize_edge(v.value());
                            results.push(Ok((node, edge)));
                        }
                    }
                }
                Err(e) => {
                    results.push(Err(PristineError::Storage(e)));
                }
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn get_file_mtime(&self, path: &str) -> PristineResult<Option<(i64, u32, u64)>> {
        let table = self.txn.open_table(FILE_MTIMES)?;
        match table.get(path)? {
            Some(value) => {
                let (secs, nanos, size) = decode_file_mtime(value.value());
                Ok(Some((secs, nanos, size)))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::Pristine;
    use tempfile::tempdir;

    #[test]
    fn test_read_empty_database() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let txn = pristine.read_txn().unwrap();

        // Empty database should return None for lookups
        assert!(txn.get_external(NodeId::new(1)).unwrap().is_none());
        assert!(txn.get_stack("main").unwrap().is_none());
        assert!(txn.get_inode("test.txt").unwrap().is_none());
        assert!(txn.list_stacks().unwrap().is_empty());
    }
}
