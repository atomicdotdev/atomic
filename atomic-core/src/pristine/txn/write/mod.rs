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

mod graph;
mod stack;
mod tree;

#[cfg(test)]
mod tests;

// MutTxnT Implementation

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

    // CRDT Table Operations

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
