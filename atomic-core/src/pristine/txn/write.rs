//! Write transaction implementation
//!
//! This module provides the `WriteTxn` struct which implements read-write
//! access to the pristine database.

use std::sync::atomic::{AtomicU64, Ordering};

use redb::{ReadableMultimapTable, ReadableTable, WriteTransaction};

use crate::crdt::tables::{
    decode_branch_value, decode_leaf_value, decode_trunk_value, decode_vertex_position,
    SerializedBranch, SerializedLeaf, SerializedTrunk, BRANCHES, BRANCH_LEAVES, BRANCH_VERTEX,
    INODE_TRUNK, LEAVES, PATH_TRUNK, TRUNKS, TRUNK_BRANCHES,
};

use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Merkle, NodeId, Position,
    SerializedGraphEdge,
};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::*;
use crate::pristine::traits::{GraphTxnT, MutTxnT, StackState, StackTxnT, TreeTxnT};

use super::helpers::{
    deserialize_edge, deserialize_stack_state, serialize_edge, serialize_stack_state, AdjIterator,
};

/// Read-write transaction
///
/// Provides read and write access to the pristine database. Only one write
/// transaction can be active at a time.
pub struct WriteTxn<'a> {
    pub(crate) txn: WriteTransaction,
    pub(crate) next_node_id: &'a AtomicU64,
    pub(crate) next_stack_id: &'a AtomicU64,
    pub(crate) next_inode: &'a AtomicU64,
}

impl<'a> WriteTxn<'a> {
    /// Create a new write transaction
    pub(crate) fn new(
        txn: WriteTransaction,
        next_node_id: &'a AtomicU64,
        next_stack_id: &'a AtomicU64,
        next_inode: &'a AtomicU64,
    ) -> Self {
        Self {
            txn,
            next_node_id,
            next_stack_id,
            next_inode,
        }
    }
}

// =============================================================================
// GraphTxnT Implementation
// =============================================================================

impl<'a> GraphTxnT for WriteTxn<'a> {
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
    /// - For non-empty vertices, finds one where start < pos <= end
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

        // Look for a span that ends at this position
        for result in table.range::<&[u8; 24]>(&start_key..&end_key)? {
            let (key, _values) = result?;
            let (v_change, v_start, v_end) = decode_vertex(key.value());

            if v_change != change_id {
                continue;
            }

            // Check for span that ends at this position
            // For predecessors, we want the span where end == target_pos
            if v_end == target_pos && v_start < v_end {
                return Ok(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }

            // Also check if position falls within [start, end]
            // This handles the case where we're looking for a span containing this position
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

impl<'a> StackTxnT for WriteTxn<'a> {
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

impl<'a> TreeTxnT for WriteTxn<'a> {
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
        let guard = table.get(path)?;
        match guard {
            Some(value) => {
                let bytes = value.value();
                let (secs, nanos, size) = decode_file_mtime(bytes);
                Ok(Some((secs, nanos, size)))
            }
            None => Ok(None),
        }
    }
}

// =============================================================================
// MutTxnT Implementation
// =============================================================================

impl<'a> MutTxnT for WriteTxn<'a> {
    fn register_change(&mut self, hash: &Hash) -> PristineResult<NodeId> {
        // Check if already registered
        {
            let table = self.txn.open_table(INTERNAL)?;
            let result = table.get(hash.as_bytes())?;
            if let Some(value) = result {
                return Ok(NodeId::new(value.value()));
            }
        }

        // Allocate a new ID
        let id = self.next_node_id.fetch_add(1, Ordering::SeqCst);
        let node_id = NodeId::new(id);

        // Insert into both tables
        {
            let mut external = self.txn.open_table(EXTERNAL)?;
            external.insert(id, hash.as_bytes())?;
        }
        {
            let mut internal = self.txn.open_table(INTERNAL)?;
            internal.insert(hash.as_bytes(), id)?;
        }
        {
            let mut node_types = self.txn.open_table(NODE_TYPES)?;
            node_types.insert(id, node_type::CHANGE)?;
        }

        Ok(node_id)
    }

    fn register_tag(&mut self, hash: &Hash) -> PristineResult<NodeId> {
        // Check if already registered
        {
            let table = self.txn.open_table(INTERNAL)?;
            let result = table.get(hash.as_bytes())?;
            if let Some(value) = result {
                return Ok(NodeId::new(value.value()));
            }
        }

        // Allocate a new ID
        let id = self.next_node_id.fetch_add(1, Ordering::SeqCst);
        let node_id = NodeId::new(id);

        // Insert into both tables
        {
            let mut external = self.txn.open_table(EXTERNAL)?;
            external.insert(id, hash.as_bytes())?;
        }
        {
            let mut internal = self.txn.open_table(INTERNAL)?;
            internal.insert(hash.as_bytes(), id)?;
        }
        {
            let mut node_types = self.txn.open_table(NODE_TYPES)?;
            node_types.insert(id, node_type::TAG)?;
        }

        Ok(node_id)
    }

    fn register_attestation(&mut self, hash: &Hash) -> PristineResult<NodeId> {
        // Check if already registered
        {
            let table = self.txn.open_table(INTERNAL)?;
            let result = table.get(hash.as_bytes())?;
            if let Some(value) = result {
                return Ok(NodeId::new(value.value()));
            }
        }

        // Allocate a new ID
        let id = self.next_node_id.fetch_add(1, Ordering::SeqCst);
        let node_id = NodeId::new(id);

        // Insert into both tables
        {
            let mut external = self.txn.open_table(EXTERNAL)?;
            external.insert(id, hash.as_bytes())?;
        }
        {
            let mut internal = self.txn.open_table(INTERNAL)?;
            internal.insert(hash.as_bytes(), id)?;
        }
        {
            let mut node_types = self.txn.open_table(NODE_TYPES)?;
            node_types.insert(id, node_type::ATTESTATION)?;
        }

        Ok(node_id)
    }

    fn put_graph(
        &mut self,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> PristineResult<bool> {
        let mut table = self.txn.open_multimap_table(GRAPH)?;
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        let value = serialize_edge(&edge);
        let inserted = table.insert(&key, &value)?;
        Ok(inserted)
    }

    fn del_graph(
        &mut self,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> PristineResult<bool> {
        let mut table = self.txn.open_multimap_table(GRAPH)?;
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        let value = serialize_edge(&edge);
        let removed = table.remove(&key, &value)?;
        Ok(removed)
    }

    fn put_inode_graph(
        &mut self,
        inode: Inode,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> PristineResult<bool> {
        let mut table = self.txn.open_multimap_table(INODE_GRAPH)?;
        let key = encode_inode_vertex(
            inode.get(),
            node.change.get(),
            node.start.get(),
            node.end.get(),
        );
        let value = serialize_edge(&edge);
        let inserted = table.insert(&key, &value)?;
        Ok(inserted)
    }

    fn del_inode_graph(
        &mut self,
        inode: Inode,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> PristineResult<bool> {
        let mut table = self.txn.open_multimap_table(INODE_GRAPH)?;
        let key = encode_inode_vertex(
            inode.get(),
            node.change.get(),
            node.start.get(),
            node.end.get(),
        );
        let value = serialize_edge(&edge);
        let removed = table.remove(&key, &value)?;
        Ok(removed)
    }

    fn open_or_create_stack(&mut self, name: &str) -> PristineResult<StackState> {
        // Check if stack exists
        {
            let table = self.txn.open_table(STACKS)?;
            let result = table.get(name)?;
            if let Some(value) = result {
                return deserialize_stack_state(value.value());
            }
        }

        // Create new stack
        let id = self.next_stack_id.fetch_add(1, Ordering::SeqCst);
        let state = StackState::new(id, name.to_string());

        // Save it
        {
            let mut table = self.txn.open_table(STACKS)?;
            let bytes = serialize_stack_state(&state);
            table.insert(name, bytes.as_slice())?;
        }

        Ok(state)
    }

    fn put_change(
        &mut self,
        stack: &mut StackState,
        change_id: NodeId,
        change_hash: &Hash,
    ) -> PristineResult<u64> {
        let seq = stack.change_count;

        // Add to change log
        {
            let mut table = self.txn.open_table(STACK_CHANGES)?;
            let key = encode_stack_seq(stack.id, seq);
            table.insert(&key, change_id.get())?;
        }

        // Add reverse mapping
        {
            let mut table = self.txn.open_table(REV_STACK_CHANGES)?;
            let key = encode_stack_seq(stack.id, change_id.get());
            table.insert(&key, seq)?;
        }

        // Update merkle state
        stack.state = stack.state.next(change_hash);
        stack.change_count += 1;

        // Store the merkle state at this sequence
        {
            let mut table = self.txn.open_table(TAGS)?;
            let key = encode_stack_seq(stack.id, seq);
            table.insert(&key, stack.state.as_bytes())?;
        }

        // Store state -> sequence mapping
        {
            let mut table = self.txn.open_table(STATES)?;
            let key = encode_stack_merkle(stack.id, stack.state.as_bytes());
            table.insert(&key, seq)?;
        }

        Ok(seq)
    }

    fn del_change(
        &mut self,
        stack: &mut StackState,
        change_id: NodeId,
        _change_hash: &Hash,
    ) -> PristineResult<Option<u64>> {
        // Find the sequence number for this change
        let seq = {
            let table = self.txn.open_table(REV_STACK_CHANGES)?;
            let key = encode_stack_seq(stack.id, change_id.get());
            let result = table.get(&key)?;
            match result {
                Some(value) => {
                    let v = value.value();
                    drop(value);
                    v
                }
                None => return Ok(None), // Change not in this stack
            }
        };

        // Remove from STACK_CHANGES
        {
            let mut table = self.txn.open_table(STACK_CHANGES)?;
            let key = encode_stack_seq(stack.id, seq);
            table.remove(&key)?;
        }

        // Remove from REV_STACK_CHANGES
        {
            let mut table = self.txn.open_table(REV_STACK_CHANGES)?;
            let key = encode_stack_seq(stack.id, change_id.get());
            table.remove(&key)?;
        }

        // Remove from TAGS (the merkle state at this sequence)
        {
            let mut table = self.txn.open_table(TAGS)?;
            let key = encode_stack_seq(stack.id, seq);
            table.remove(&key)?;
        }

        // Shift all subsequent changes down by 1
        // We need to update sequences from seq+1 to change_count-1
        let original_count = stack.change_count;
        for s in (seq + 1)..original_count {
            // Get the change_id at this sequence
            let cid = {
                let table = self.txn.open_table(STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, s);
                let result = table.get(&key)?;
                match result {
                    Some(v) => {
                        let id = NodeId::new(v.value());
                        drop(v);
                        id
                    }
                    None => continue,
                }
            };

            // Remove old entry
            {
                let mut table = self.txn.open_table(STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, s);
                table.remove(&key)?;
            }

            // Insert at new sequence (s - 1)
            {
                let mut table = self.txn.open_table(STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, s - 1);
                table.insert(&key, cid.get())?;
            }

            // Update reverse mapping
            {
                let mut table = self.txn.open_table(REV_STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, cid.get());
                table.insert(&key, s - 1)?;
            }
        }

        // Decrement change count
        stack.change_count -= 1;

        // Recompute merkle state from scratch
        stack.state = Merkle::ZERO;
        for s in 0..stack.change_count {
            let cid = {
                let table = self.txn.open_table(STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, s);
                let result = table.get(&key)?;
                match result {
                    Some(v) => {
                        let id = NodeId::new(v.value());
                        drop(v);
                        id
                    }
                    None => continue,
                }
            };

            let hash = self
                .get_external(cid)?
                .ok_or_else(|| PristineError::ChangeNotFound { id: cid.get() })?;

            stack.state = stack.state.next(&hash);

            // Update TAGS with the new merkle state at this sequence
            {
                let mut table = self.txn.open_table(TAGS)?;
                let key = encode_stack_seq(stack.id, s);
                table.insert(&key, stack.state.as_bytes())?;
            }

            // Update STATES mapping
            {
                let mut table = self.txn.open_table(STATES)?;
                let key = encode_stack_merkle(stack.id, stack.state.as_bytes());
                table.insert(&key, s)?;
            }
        }

        Ok(Some(seq))
    }

    fn reinsert_change(
        &mut self,
        stack: &mut StackState,
        change_id: NodeId,
        change_hash: &Hash,
        at_sequence: u64,
    ) -> PristineResult<()> {
        // Clamp sequence to valid range
        let insert_at = at_sequence.min(stack.change_count);

        // Shift all changes from insert_at onwards up by 1
        // Work backwards to avoid overwriting
        for s in (insert_at..stack.change_count).rev() {
            // Get the change_id at this sequence
            let cid = {
                let table = self.txn.open_table(STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, s);
                let result = table.get(&key)?;
                match result {
                    Some(v) => {
                        let id = NodeId::new(v.value());
                        drop(v);
                        id
                    }
                    None => continue,
                }
            };

            // Remove old entry
            {
                let mut table = self.txn.open_table(STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, s);
                table.remove(&key)?;
            }

            // Insert at new sequence (s + 1)
            {
                let mut table = self.txn.open_table(STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, s + 1);
                table.insert(&key, cid.get())?;
            }

            // Update reverse mapping
            {
                let mut table = self.txn.open_table(REV_STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, cid.get());
                table.insert(&key, s + 1)?;
            }
        }

        // Insert the new change at the specified position
        {
            let mut table = self.txn.open_table(STACK_CHANGES)?;
            let key = encode_stack_seq(stack.id, insert_at);
            table.insert(&key, change_id.get())?;
        }

        // Add reverse mapping
        {
            let mut table = self.txn.open_table(REV_STACK_CHANGES)?;
            let key = encode_stack_seq(stack.id, change_id.get());
            table.insert(&key, insert_at)?;
        }

        // Increment change count
        stack.change_count += 1;

        // Recompute merkle state from scratch
        stack.state = Merkle::ZERO;
        for s in 0..stack.change_count {
            let cid = {
                let table = self.txn.open_table(STACK_CHANGES)?;
                let key = encode_stack_seq(stack.id, s);
                let result = table.get(&key)?;
                match result {
                    Some(v) => {
                        let id = NodeId::new(v.value());
                        drop(v);
                        id
                    }
                    None => continue,
                }
            };

            let hash = if cid == change_id {
                *change_hash
            } else {
                self.get_external(cid)?
                    .ok_or_else(|| PristineError::ChangeNotFound { id: cid.get() })?
            };

            stack.state = stack.state.next(&hash);

            // Update TAGS with the new merkle state at this sequence
            {
                let mut table = self.txn.open_table(TAGS)?;
                let key = encode_stack_seq(stack.id, s);
                table.insert(&key, stack.state.as_bytes())?;
            }

            // Update STATES mapping
            {
                let mut table = self.txn.open_table(STATES)?;
                let key = encode_stack_merkle(stack.id, stack.state.as_bytes());
                table.insert(&key, s)?;
            }
        }

        Ok(())
    }

    fn update_stack(&mut self, stack: &StackState) -> PristineResult<()> {
        let mut table = self.txn.open_table(STACKS)?;
        let bytes = serialize_stack_state(stack);
        table.insert(stack.name.as_str(), bytes.as_slice())?;
        Ok(())
    }

    fn del_stack(&mut self, stack: &StackState) -> PristineResult<()> {
        // Remove from STACKS table
        {
            let mut table = self.txn.open_table(STACKS)?;
            table.remove(stack.name.as_str())?;
        }

        // Remove all change log entries for this stack
        // We need to iterate through all sequences and remove them
        {
            let mut table = self.txn.open_table(STACK_CHANGES)?;
            for seq in 0..stack.change_count {
                let key = encode_stack_seq(stack.id, seq);
                table.remove(&key)?;
            }
        }

        // Remove all reverse change log entries
        // We need to read STACK_CHANGES first to know which change_ids to remove
        {
            let mut rev_table = self.txn.open_table(REV_STACK_CHANGES)?;
            // Since we don't have the change_ids readily available, we iterate
            // through the stack_changes we're about to delete
            let table = self.txn.open_table(STACK_CHANGES)?;
            for seq in 0..stack.change_count {
                let key = encode_stack_seq(stack.id, seq);
                if let Some(change_id) = table.get(&key)? {
                    let rev_key = encode_stack_seq(stack.id, change_id.value());
                    rev_table.remove(&rev_key)?;
                }
            }
        }

        // Remove all state/sequence mappings from STATES table
        // We need to remove entries where the key starts with this stack's id
        // The key format is (stack_id: u64, merkle: [u8; 32])
        {
            let mut table = self.txn.open_table(STATES)?;
            // Get all the merkle states from TAGS and remove their STATES entries
            let tags_table = self.txn.open_table(TAGS)?;
            for seq in 0..stack.change_count {
                let key = encode_stack_seq(stack.id, seq);
                if let Some(merkle_bytes) = tags_table.get(&key)? {
                    let merkle = merkle_bytes.value();
                    let state_key = encode_stack_merkle(stack.id, merkle);
                    table.remove(&state_key)?;
                }
            }
        }

        // Remove all tag entries from TAGS table
        {
            let mut table = self.txn.open_table(TAGS)?;
            for seq in 0..stack.change_count {
                let key = encode_stack_seq(stack.id, seq);
                table.remove(&key)?;
            }
        }

        Ok(())
    }

    fn put_tree(&mut self, path: &str, inode: Inode) -> PristineResult<()> {
        {
            let mut table = self.txn.open_table(TREE)?;
            table.insert(path, inode.get())?;
        }
        {
            let mut table = self.txn.open_table(REV_TREE)?;
            table.insert(inode.get(), path)?;
        }
        Ok(())
    }

    fn del_tree(&mut self, path: &str) -> PristineResult<Option<Inode>> {
        let inode = {
            let mut table = self.txn.open_table(TREE)?;
            let removed = table.remove(path)?;
            match removed {
                Some(value) => Some(Inode::new(value.value())),
                None => None,
            }
        };

        if let Some(inode) = inode {
            let mut table = self.txn.open_table(REV_TREE)?;
            table.remove(inode.get())?;
        }

        Ok(inode)
    }

    fn put_file_mtime(
        &mut self,
        path: &str,
        mtime_secs: i64,
        mtime_nanos: u32,
        file_size: u64,
    ) -> Result<(), PristineError> {
        let value = encode_file_mtime(mtime_secs, mtime_nanos, file_size);
        let mut table = self.txn.open_table(FILE_MTIMES)?;
        table.insert(path, &value)?;
        Ok(())
    }

    fn del_file_mtime(&mut self, path: &str) -> Result<(), PristineError> {
        let mut table = self.txn.open_table(FILE_MTIMES)?;
        table.remove(path)?;
        Ok(())
    }

    fn put_inode(&mut self, inode: Inode, pos: Position<NodeId>) -> PristineResult<()> {
        let pos_bytes = encode_position(pos.change.get(), pos.pos.get());
        {
            let mut table = self.txn.open_table(INODES)?;
            table.insert(inode.get(), &pos_bytes)?;
        }
        {
            let mut table = self.txn.open_table(REV_INODES)?;
            table.insert(&pos_bytes, inode.get())?;
        }
        Ok(())
    }

    fn del_inode(&mut self, inode: Inode) -> PristineResult<Option<Position<NodeId>>> {
        let pos = {
            let mut table = self.txn.open_table(INODES)?;
            let removed = table.remove(inode.get())?;
            match removed {
                Some(value) => {
                    let (change_id, pos) = decode_position(value.value());
                    Some(Position::new(
                        NodeId::new(change_id),
                        ChangePosition::new(pos),
                    ))
                }
                None => None,
            }
        };

        if let Some(ref p) = pos {
            let pos_bytes = encode_position(p.change.get(), p.pos.get());
            let mut table = self.txn.open_table(REV_INODES)?;
            table.remove(&pos_bytes)?;
        }

        Ok(pos)
    }

    fn put_dep(&mut self, change_id: NodeId, dep_id: NodeId) -> PristineResult<()> {
        {
            let mut table = self.txn.open_multimap_table(DEPS)?;
            table.insert(change_id.get(), dep_id.get())?;
        }
        {
            let mut table = self.txn.open_multimap_table(REV_DEPS)?;
            table.insert(dep_id.get(), change_id.get())?;
        }
        Ok(())
    }

    fn get_deps(&self, change_id: NodeId) -> PristineResult<Vec<NodeId>> {
        let table = self.txn.open_multimap_table(DEPS)?;
        let mut deps = Vec::new();
        for result in table.get(change_id.get())? {
            if let Ok(v) = result {
                deps.push(NodeId::new(v.value()));
            }
        }
        Ok(deps)
    }

    fn alloc_inode(&mut self) -> PristineResult<Inode> {
        let id = self.next_inode.fetch_add(1, Ordering::SeqCst);
        Ok(Inode::new(id))
    }

    fn put_directory(&mut self, inode: Inode, flags: u8) -> PristineResult<()> {
        let mut table = self.txn.open_table(DIRECTORIES)?;
        table.insert(inode.get(), flags)?;
        Ok(())
    }

    fn del_directory(&mut self, inode: Inode) -> PristineResult<Option<u8>> {
        let mut table = self.txn.open_table(DIRECTORIES)?;
        let result = table.remove(inode.get())?;
        Ok(result.map(|v| v.value()))
    }

    // =========================================================================
    // CRDT Table Operations
    // =========================================================================

    fn put_crdt_trunk(&mut self, key: &[u8; 12], value: &[u8]) -> PristineResult<()> {
        let mut table = self.txn.open_table(TRUNKS)?;
        table.insert(key, value)?;
        Ok(())
    }

    fn get_crdt_trunk(&mut self, key: &[u8; 12]) -> PristineResult<Option<SerializedTrunk>> {
        let table = self.txn.open_table(TRUNKS)?;
        let result = match table.get(key)? {
            Some(guard) => {
                let bytes = guard.value();
                // Copy the bytes before the guard is dropped
                let bytes_copy: Vec<u8> = bytes.to_vec();
                decode_trunk_value(&bytes_copy)
            }
            None => None,
        };
        Ok(result)
    }

    fn put_crdt_inode_trunk(&mut self, inode: u64, trunk_key: &[u8; 12]) -> PristineResult<()> {
        let mut table = self.txn.open_table(INODE_TRUNK)?;
        table.insert(inode, trunk_key)?;
        Ok(())
    }

    fn put_crdt_path_trunk(&mut self, path: &str, trunk_key: &[u8; 12]) -> PristineResult<()> {
        let mut table = self.txn.open_table(PATH_TRUNK)?;
        table.insert(path, trunk_key)?;
        Ok(())
    }

    fn del_crdt_path_trunk(&mut self, path: &str) -> PristineResult<()> {
        let mut table = self.txn.open_table(PATH_TRUNK)?;
        table.remove(path)?;
        Ok(())
    }

    fn put_crdt_branch(&mut self, key: &[u8; 12], value: &[u8; 24]) -> PristineResult<()> {
        let mut table = self.txn.open_table(BRANCHES)?;
        table.insert(key, value)?;
        Ok(())
    }

    fn get_crdt_branch(&mut self, key: &[u8; 12]) -> PristineResult<Option<SerializedBranch>> {
        let table = self.txn.open_table(BRANCHES)?;
        let result = match table.get(key)? {
            Some(guard) => {
                let bytes: [u8; 24] = *guard.value();
                Some(decode_branch_value(&bytes))
            }
            None => None,
        };
        Ok(result)
    }

    fn put_crdt_trunk_branch(
        &mut self,
        trunk_key: &[u8; 12],
        branch_key: &[u8; 12],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_multimap_table(TRUNK_BRANCHES)?;
        table.insert(trunk_key, branch_key)?;
        Ok(())
    }

    fn put_crdt_leaf(&mut self, key: &[u8; 12], value: &[u8; 22]) -> PristineResult<()> {
        let mut table = self.txn.open_table(LEAVES)?;
        table.insert(key, value)?;
        Ok(())
    }

    fn get_crdt_leaf(&mut self, key: &[u8; 12]) -> PristineResult<Option<SerializedLeaf>> {
        let table = self.txn.open_table(LEAVES)?;
        let result = match table.get(key)? {
            Some(guard) => {
                let bytes: [u8; 22] = *guard.value();
                Some(decode_leaf_value(&bytes))
            }
            None => None,
        };
        Ok(result)
    }

    fn put_crdt_branch_leaf(
        &mut self,
        branch_key: &[u8; 12],
        leaf_key: &[u8; 12],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_multimap_table(BRANCH_LEAVES)?;
        table.insert(branch_key, leaf_key)?;
        Ok(())
    }

    fn get_trunk_by_path(&mut self, path: &str) -> PristineResult<Option<crate::crdt::TrunkId>> {
        use crate::crdt::tables::decode_trunk_id;

        let table = self.txn.open_table(PATH_TRUNK)?;
        let result = table.get(path)?;
        match result {
            Some(guard) => {
                let key: [u8; 12] = *guard.value();
                drop(guard); // Explicitly drop the guard before returning
                Ok(Some(decode_trunk_id(&key)))
            }
            None => Ok(None),
        }
    }

    fn iter_trunk_branches(
        &mut self,
        trunk_key: &[u8; 12],
    ) -> PristineResult<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>> {
        let table = self.txn.open_multimap_table(TRUNK_BRANCHES)?;

        // Collect all branch keys for this trunk
        let mut results: Vec<Result<[u8; 12], PristineError>> = Vec::new();

        // Get all values for this trunk key
        let values = table.get(trunk_key)?;
        for value_result in values {
            match value_result {
                Ok(access) => {
                    let bytes: [u8; 12] = *access.value();
                    results.push(Ok(bytes));
                }
                Err(e) => {
                    results.push(Err(PristineError::Storage(e)));
                }
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn iter_branch_leaves(
        &mut self,
        branch_key: &[u8; 12],
    ) -> PristineResult<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>> {
        let table = self.txn.open_multimap_table(BRANCH_LEAVES)?;

        // Collect all leaf keys for this branch
        let mut results: Vec<Result<[u8; 12], PristineError>> = Vec::new();

        // Get all values for this branch key
        let values = table.get(branch_key)?;
        for value_result in values {
            match value_result {
                Ok(access) => {
                    let bytes: [u8; 12] = *access.value();
                    results.push(Ok(bytes));
                }
                Err(e) => {
                    results.push(Err(PristineError::Storage(e)));
                }
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn put_crdt_branch_vertex(
        &mut self,
        branch_key: &[u8; 12],
        node_bytes: &[u8; 24],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_table(BRANCH_VERTEX)?;
        table.insert(branch_key, node_bytes)?;
        Ok(())
    }

    fn get_crdt_branch_vertex(
        &mut self,
        branch_key: &[u8; 12],
    ) -> PristineResult<Option<GraphNode<NodeId>>> {
        let table = self.txn.open_table(BRANCH_VERTEX)?;
        let result = table.get(branch_key)?;
        match result {
            Some(value) => {
                // Copy the bytes out while the guard is still alive
                let bytes: [u8; 24] = *value.value();
                drop(value); // Explicitly drop the guard
                Ok(Some(decode_vertex_position(&bytes)))
            }
            None => Ok(None),
        }
    }

    fn put_inodes(&mut self, inode: u64, pos: &Position<NodeId>) -> PristineResult<()> {
        let mut inodes_table = self.txn.open_table(INODES)?;
        let mut rev_inodes_table = self.txn.open_table(REV_INODES)?;

        // Encode position as 16 bytes: change_id (8) + pos (8)
        let mut pos_bytes = [0u8; 16];
        pos_bytes[0..8].copy_from_slice(&pos.change.get().to_le_bytes());
        pos_bytes[8..16].copy_from_slice(&pos.pos.get().to_le_bytes());

        inodes_table.insert(inode, &pos_bytes)?;
        rev_inodes_table.insert(&pos_bytes, inode)?;
        Ok(())
    }

    fn commit(self) -> PristineResult<()> {
        self.txn.commit()?;
        Ok(())
    }

    fn abort(self) -> PristineResult<()> {
        self.txn.abort()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::traits::VertexExt;
    use crate::pristine::Pristine;
    use tempfile::tempdir;

    #[test]
    fn test_register_change() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let hash = Hash::of(b"test change");
        let id = txn.register_change(&hash).unwrap();

        // Should get same ID for same hash
        let id2 = txn.register_change(&hash).unwrap();
        assert_eq!(id, id2);

        // Should be able to look up both ways
        assert_eq!(txn.get_external(id).unwrap(), Some(hash));
        assert_eq!(txn.get_internal(&hash).unwrap(), Some(id));

        txn.commit().unwrap();
    }

    #[test]
    fn test_stack_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Create a stack
        let mut stack = txn.open_or_create_stack("main").unwrap();
        assert_eq!(stack.name, "main");
        assert_eq!(stack.change_count, 0);

        // Add a change
        let hash = Hash::of(b"change 1");
        let change_id = txn.register_change(&hash).unwrap();
        let seq = txn.put_change(&mut stack, change_id, &hash).unwrap();
        assert_eq!(seq, 0);
        assert_eq!(stack.change_count, 1);

        // Update stack state
        txn.update_stack(&stack).unwrap();

        // List stacks
        let stacks = txn.list_stacks().unwrap();
        assert_eq!(stacks, vec!["main"]);

        txn.commit().unwrap();

        // Read back
        let txn = pristine.read_txn().unwrap();
        let stack = txn.get_stack("main").unwrap().unwrap();
        assert_eq!(stack.change_count, 1);
    }

    #[test]
    fn test_register_tag() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let hash = Hash::of(b"test tag content");
        let id = txn.register_tag(&hash).unwrap();

        // Should get same ID for same hash
        let id2 = txn.register_tag(&hash).unwrap();
        assert_eq!(id, id2);

        // Should be able to look up both ways
        assert_eq!(txn.get_external(id).unwrap(), Some(hash));
        assert_eq!(txn.get_internal(&hash).unwrap(), Some(id));

        // Should be marked as a tag type
        assert_eq!(txn.get_node_type(id).unwrap(), Some(node_type::TAG));

        txn.commit().unwrap();
    }

    #[test]
    fn test_get_node_type() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Register a change
        let change_hash = Hash::of(b"change content");
        let change_id = txn.register_change(&change_hash).unwrap();

        // Register a tag
        let tag_hash = Hash::of(b"tag content");
        let tag_id = txn.register_tag(&tag_hash).unwrap();

        // Verify node types
        assert_eq!(
            txn.get_node_type(change_id).unwrap(),
            Some(node_type::CHANGE)
        );
        assert_eq!(txn.get_node_type(tag_id).unwrap(), Some(node_type::TAG));

        // Non-existent node should return None
        let fake_id = NodeId::new(99999);
        assert_eq!(txn.get_node_type(fake_id).unwrap(), None);

        txn.commit().unwrap();
    }

    #[test]
    fn test_change_and_tag_different_types() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Same content registered as change and tag should get different IDs
        // because we use different registration methods
        let content = b"shared content";
        let hash = Hash::of(content);

        let change_id = txn.register_change(&hash).unwrap();

        // Registering as tag should return the existing ID (since hash is same)
        let tag_id = txn.register_tag(&hash).unwrap();

        // Same hash means same ID
        assert_eq!(change_id, tag_id);

        // The node type should be CHANGE since it was registered first
        assert_eq!(
            txn.get_node_type(change_id).unwrap(),
            Some(node_type::CHANGE)
        );

        txn.commit().unwrap();
    }

    #[test]
    fn test_del_stack() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Create a stack with some changes
        {
            let mut txn = pristine.write_txn().unwrap();

            let mut stack = txn.open_or_create_stack("to-delete").unwrap();
            assert_eq!(stack.name, "to-delete");

            // Add some changes
            let hash1 = Hash::of(b"change 1");
            let hash2 = Hash::of(b"change 2");
            let change_id1 = txn.register_change(&hash1).unwrap();
            let change_id2 = txn.register_change(&hash2).unwrap();

            txn.put_change(&mut stack, change_id1, &hash1).unwrap();
            txn.put_change(&mut stack, change_id2, &hash2).unwrap();
            txn.update_stack(&stack).unwrap();

            assert_eq!(stack.change_count, 2);

            // Verify stack exists
            let stacks = txn.list_stacks().unwrap();
            assert!(stacks.contains(&"to-delete".to_string()));

            txn.commit().unwrap();
        }

        // Delete the stack
        {
            let mut txn = pristine.write_txn().unwrap();

            let stack = txn.get_stack("to-delete").unwrap().unwrap();
            txn.del_stack(&stack).unwrap();

            txn.commit().unwrap();
        }

        // Verify stack is gone
        {
            let txn = pristine.read_txn().unwrap();

            let stack = txn.get_stack("to-delete").unwrap();
            assert!(stack.is_none());

            let stacks = txn.list_stacks().unwrap();
            assert!(!stacks.contains(&"to-delete".to_string()));
        }
    }

    #[test]
    fn test_del_stack_preserves_other_stacks() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Create two stacks
        {
            let mut txn = pristine.write_txn().unwrap();

            let mut stack1 = txn.open_or_create_stack("keep-me").unwrap();
            let mut stack2 = txn.open_or_create_stack("delete-me").unwrap();

            // Add changes to both
            let hash1 = Hash::of(b"change for keep");
            let hash2 = Hash::of(b"change for delete");
            let change_id1 = txn.register_change(&hash1).unwrap();
            let change_id2 = txn.register_change(&hash2).unwrap();

            txn.put_change(&mut stack1, change_id1, &hash1).unwrap();
            txn.put_change(&mut stack2, change_id2, &hash2).unwrap();
            txn.update_stack(&stack1).unwrap();
            txn.update_stack(&stack2).unwrap();

            txn.commit().unwrap();
        }

        // Delete only one stack
        {
            let mut txn = pristine.write_txn().unwrap();

            let stack = txn.get_stack("delete-me").unwrap().unwrap();
            txn.del_stack(&stack).unwrap();

            txn.commit().unwrap();
        }

        // Verify the other stack is intact
        {
            let txn = pristine.read_txn().unwrap();

            // Deleted stack should be gone
            assert!(txn.get_stack("delete-me").unwrap().is_none());

            // Other stack should still exist with its change
            let stack = txn.get_stack("keep-me").unwrap().unwrap();
            assert_eq!(stack.change_count, 1);
        }
    }

    #[test]
    fn test_tree_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let inode = txn.alloc_inode().unwrap();
        txn.put_tree("src/main.rs", inode).unwrap();

        assert_eq!(txn.get_inode("src/main.rs").unwrap(), Some(inode));
        assert_eq!(
            txn.get_path(inode).unwrap(),
            Some("src/main.rs".to_string())
        );

        let removed = txn.del_tree("src/main.rs").unwrap();
        assert_eq!(removed, Some(inode));
        assert_eq!(txn.get_inode("src/main.rs").unwrap(), None);

        txn.commit().unwrap();
    }

    #[test]
    fn test_graph_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let node = GraphNode::from_parts(NodeId::new(1), 0, 100);
        let dest = Position::new(NodeId::new(2), ChangePosition::new(0));
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(1));

        // Insert edge
        txn.put_graph(node, edge).unwrap();

        // Check it exists
        assert!(txn.has_vertex(node).unwrap());

        // Get edges
        let edges = txn.get_edges(node).unwrap();
        assert_eq!(edges.len(), 1);

        // Delete edge
        txn.del_graph(node, edge).unwrap();

        txn.commit().unwrap();
    }

    #[test]
    fn test_del_change() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Create a stack with 3 changes
        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");
        let hash3 = Hash::of(b"change 3");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();
        let id3 = txn.register_change(&hash3).unwrap();

        txn.put_change(&mut stack, id1, &hash1).unwrap();
        txn.put_change(&mut stack, id2, &hash2).unwrap();
        txn.put_change(&mut stack, id3, &hash3).unwrap();
        txn.update_stack(&stack).unwrap();

        assert_eq!(stack.change_count, 3);

        // Remove the middle change (id2)
        let removed_seq = txn.del_change(&mut stack, id2, &hash2).unwrap();
        assert_eq!(removed_seq, Some(1)); // Was at sequence 1

        // Stack should now have 2 changes
        assert_eq!(stack.change_count, 2);

        // Change 1 should still be at sequence 0
        let seq0 = txn.get_change_at_seq(&stack, 0).unwrap();
        assert_eq!(seq0, Some(id1));

        // Change 3 should now be at sequence 1 (shifted down)
        let seq1 = txn.get_change_at_seq(&stack, 1).unwrap();
        assert_eq!(seq1, Some(id3));

        // Merkle state should be recomputed
        let expected_state = Merkle::ZERO.next(&hash1).next(&hash3);
        assert_eq!(stack.state, expected_state);

        txn.update_stack(&stack).unwrap();
        txn.commit().unwrap();
    }

    #[test]
    fn test_del_change_not_in_stack() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();

        // Only add change 1 to the stack
        txn.put_change(&mut stack, id1, &hash1).unwrap();

        // Try to remove change 2 (not in stack)
        let result = txn.del_change(&mut stack, id2, &hash2).unwrap();
        assert_eq!(result, None);

        // Stack should be unchanged
        assert_eq!(stack.change_count, 1);

        txn.commit().unwrap();
    }

    #[test]
    fn test_reinsert_change() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Create a stack with 2 changes
        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");
        let hash3 = Hash::of(b"change 3");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();
        let id3 = txn.register_change(&hash3).unwrap();

        txn.put_change(&mut stack, id1, &hash1).unwrap();
        txn.put_change(&mut stack, id2, &hash2).unwrap();
        txn.update_stack(&stack).unwrap();

        assert_eq!(stack.change_count, 2);

        // Insert change 3 at position 1 (between change 1 and 2)
        txn.reinsert_change(&mut stack, id3, &hash3, 1).unwrap();

        // Stack should now have 3 changes
        assert_eq!(stack.change_count, 3);

        // Verify order: 1, 3, 2
        assert_eq!(txn.get_change_at_seq(&stack, 0).unwrap(), Some(id1));
        assert_eq!(txn.get_change_at_seq(&stack, 1).unwrap(), Some(id3));
        assert_eq!(txn.get_change_at_seq(&stack, 2).unwrap(), Some(id2));

        // Merkle state should be recomputed
        let expected_state = Merkle::ZERO.next(&hash1).next(&hash3).next(&hash2);
        assert_eq!(stack.state, expected_state);

        txn.update_stack(&stack).unwrap();
        txn.commit().unwrap();
    }

    #[test]
    fn test_reinsert_change_at_end() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();

        txn.put_change(&mut stack, id1, &hash1).unwrap();
        txn.update_stack(&stack).unwrap();

        // Insert at a position beyond current count (should append)
        txn.reinsert_change(&mut stack, id2, &hash2, 100).unwrap();

        assert_eq!(stack.change_count, 2);
        assert_eq!(txn.get_change_at_seq(&stack, 1).unwrap(), Some(id2));

        txn.commit().unwrap();
    }

    #[test]
    fn test_unrecord_and_reinsert_workflow() {
        // This test simulates the Gerrit-like workflow:
        // 1. Create stack with 3 changes
        // 2. Unrecord the middle one
        // 3. Reinsert it at its original position
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");
        let hash3 = Hash::of(b"change 3");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();
        let id3 = txn.register_change(&hash3).unwrap();

        txn.put_change(&mut stack, id1, &hash1).unwrap();
        txn.put_change(&mut stack, id2, &hash2).unwrap();
        txn.put_change(&mut stack, id3, &hash3).unwrap();
        txn.update_stack(&stack).unwrap();

        let original_state = stack.state;

        // Unrecord the middle change
        let original_seq = txn.del_change(&mut stack, id2, &hash2).unwrap().unwrap();
        assert_eq!(original_seq, 1);
        assert_eq!(stack.change_count, 2);

        // Reinsert at original position
        txn.reinsert_change(&mut stack, id2, &hash2, original_seq)
            .unwrap();
        assert_eq!(stack.change_count, 3);

        // State should be identical to before
        assert_eq!(stack.state, original_state);

        // Order should be restored
        assert_eq!(txn.get_change_at_seq(&stack, 0).unwrap(), Some(id1));
        assert_eq!(txn.get_change_at_seq(&stack, 1).unwrap(), Some(id2));
        assert_eq!(txn.get_change_at_seq(&stack, 2).unwrap(), Some(id3));

        txn.commit().unwrap();
    }

    #[test]
    fn test_dependency_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let change1 = NodeId::new(1);
        let change2 = NodeId::new(2);
        let change3 = NodeId::new(3);

        // change2 and change3 depend on change1
        txn.put_dep(change2, change1).unwrap();
        txn.put_dep(change3, change1).unwrap();

        // Check dependencies
        let deps2 = txn.get_deps(change2).unwrap();
        assert_eq!(deps2, vec![change1]);

        // Check reverse dependencies
        let rev_deps1 = txn.get_rev_deps(change1).unwrap();
        assert!(rev_deps1.contains(&change2));
        assert!(rev_deps1.contains(&change3));

        txn.commit().unwrap();
    }

    #[test]
    fn test_inode_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let inode = txn.alloc_inode().unwrap();
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));

        txn.put_inode(inode, pos).unwrap();

        assert_eq!(txn.inode_position(inode).unwrap(), Some(pos));
        assert_eq!(txn.position_inode(pos).unwrap(), Some(inode));

        let removed_pos = txn.del_inode(inode).unwrap();
        assert_eq!(removed_pos, Some(pos));
        assert_eq!(txn.inode_position(inode).unwrap(), None);

        txn.commit().unwrap();
    }

    #[test]
    fn test_abort_transaction() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Create a stack, then abort
        {
            let mut txn = pristine.write_txn().unwrap();
            txn.open_or_create_stack("test_stack").unwrap();
            txn.abort().unwrap();
        }

        // Stack should not exist
        let txn = pristine.read_txn().unwrap();
        assert!(txn.get_stack("test_stack").unwrap().is_none());
    }

    // =========================================================================
    // Directory Tracking Tests
    // =========================================================================

    #[test]
    fn test_directory_put_and_get() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let inode = txn.alloc_inode().unwrap();

        // Initially not a directory
        assert!(txn.get_directory_flags(inode).unwrap().is_none());
        assert!(!TreeTxnT::is_directory(&txn, inode).unwrap());

        // Mark as directory
        use crate::pristine::tables::directory_flags;
        txn.put_directory(inode, directory_flags::explicit_empty())
            .unwrap();

        // Now it's a directory
        assert!(TreeTxnT::is_directory(&txn, inode).unwrap());
        let flags = txn.get_directory_flags(inode).unwrap().unwrap();
        assert!(directory_flags::is_explicit(flags));
        assert!(directory_flags::is_empty(flags));

        txn.commit().unwrap();
    }

    #[test]
    fn test_directory_del() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let inode = txn.alloc_inode().unwrap();

        use crate::pristine::tables::directory_flags;
        txn.put_directory(inode, directory_flags::DIR_EXPLICIT)
            .unwrap();

        // Delete the directory marker
        let old_flags = txn.del_directory(inode).unwrap();
        assert_eq!(old_flags, Some(directory_flags::DIR_EXPLICIT));

        // No longer a directory
        assert!(!TreeTxnT::is_directory(&txn, inode).unwrap());

        txn.commit().unwrap();
    }

    #[test]
    fn test_directory_update_flags() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let inode = txn.alloc_inode().unwrap();

        use crate::pristine::tables::directory_flags;

        // Start as empty directory
        txn.put_directory(inode, directory_flags::explicit_empty())
            .unwrap();

        let flags = txn.get_directory_flags(inode).unwrap().unwrap();
        assert!(directory_flags::is_empty(flags));

        // Update to non-empty (file was added)
        txn.update_directory_flags(inode, directory_flags::explicit_with_children())
            .unwrap();

        let flags = txn.get_directory_flags(inode).unwrap().unwrap();
        assert!(!directory_flags::is_empty(flags));
        assert!(directory_flags::is_explicit(flags));

        txn.commit().unwrap();
    }

    #[test]
    fn test_directory_persistence() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let inode;

        use crate::pristine::tables::directory_flags;

        // Create directory in first transaction
        {
            let pristine = Pristine::open(&db_path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            inode = txn.alloc_inode().unwrap();
            txn.put_directory(inode, directory_flags::explicit_empty())
                .unwrap();
            txn.commit().unwrap();
        }

        // Verify in new transaction
        {
            let pristine = Pristine::open(&db_path).unwrap();
            let txn = pristine.write_txn().unwrap();
            assert!(TreeTxnT::is_directory(&txn, inode).unwrap());
            let flags = txn.get_directory_flags(inode).unwrap().unwrap();
            assert!(directory_flags::is_explicit(flags));
            assert!(directory_flags::is_empty(flags));
        }
    }

    #[test]
    fn test_directory_multiple_inodes() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        use crate::pristine::tables::directory_flags;

        // Create multiple directories with different flags
        let dir1 = txn.alloc_inode().unwrap();
        let dir2 = txn.alloc_inode().unwrap();
        let file = txn.alloc_inode().unwrap();

        txn.put_directory(dir1, directory_flags::explicit_empty())
            .unwrap();
        txn.put_directory(dir2, directory_flags::explicit_with_children())
            .unwrap();
        // file is not marked as directory

        assert!(TreeTxnT::is_directory(&txn, dir1).unwrap());
        assert!(TreeTxnT::is_directory(&txn, dir2).unwrap());
        assert!(!TreeTxnT::is_directory(&txn, file).unwrap());

        let flags1 = txn.get_directory_flags(dir1).unwrap().unwrap();
        let flags2 = txn.get_directory_flags(dir2).unwrap().unwrap();

        assert!(directory_flags::is_empty(flags1));
        assert!(!directory_flags::is_empty(flags2));

        txn.commit().unwrap();
    }
}
