//! Write transaction implementation
//!
//! This module provides the `WriteTxn` struct which implements read-write
//! access to the pristine database.

use std::sync::atomic::{AtomicU64, Ordering};

use redb::{ReadableMultimapTable, ReadableTable, WriteTransaction};

use crate::crdt::tables::{
    decode_branch_id, decode_branch_value, decode_leaf_value, decode_trunk_value,
    decode_vertex_position, SerializedBranch, SerializedLeaf, SerializedTrunk, BRANCHES,
    BRANCH_LEAVES, BRANCH_VERTEX, INODE_TRUNK, LEAVES, PATH_TRUNK, TRUNKS, TRUNK_BRANCHES,
    VERTEX_BRANCH,
};

use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Merkle, NodeId, Position,
    SerializedGraphEdge,
};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::*;
use crate::pristine::traits::{GraphTxnT, MutTxnT, TreeTxnT, ViewScope, ViewState, ViewTxnT};

use super::helpers::{
    deserialize_edge, deserialize_view_state, serialize_edge, serialize_view_state, AdjIterator,
};

/// Read-write transaction
///
/// Provides read and write access to the pristine database. Only one write
/// transaction can be active at a time.
pub struct WriteTxn<'a> {
    pub(crate) txn: WriteTransaction,
    pub(crate) next_node_id: &'a AtomicU64,
    pub(crate) next_view_id: &'a AtomicU64,
    pub(crate) next_inode: &'a AtomicU64,
}

impl<'a> WriteTxn<'a> {
    /// Create a new write transaction
    pub(crate) fn new(
        txn: WriteTransaction,
        next_node_id: &'a AtomicU64,
        next_view_id: &'a AtomicU64,
        next_inode: &'a AtomicU64,
    ) -> Self {
        Self {
            txn,
            next_node_id,
            next_view_id,
            next_inode,
        }
    }

    /// Populate the session tables from a Sherpa provenance graph.
    ///
    /// Called during `save_provenance_graph` when the graph has
    /// `profile == "sherpa-trace/1.0.0"`. Extracts session data from
    /// the provenance nodes' `detail` fields and writes to the four
    /// session tables.
    ///
    /// Best-effort: parse errors on individual nodes are logged and skipped.
    pub fn populate_session_tables(
        &mut self,
        provenance_id: u64,
        graph: &crate::change::ProvenanceGraph,
    ) -> PristineResult<()> {
        use crate::change::provenance_graph::ProvenanceNodeKind;
        use crate::change::session::*;

        // Gate on profile — only populate for Sherpa graphs.
        match graph.profile.as_deref() {
            Some(crate::change::provenance_graph::SHERPA_PROFILE) => {}
            _ => return Ok(()),
        }

        // Open all four tables.
        let mut events_table = self.txn.open_table(SESSION_EVENTS)?;
        let mut todos_table = self.txn.open_table(SESSION_TODOS)?;
        let mut phases_table = self.txn.open_table(SESSION_PHASES)?;
        let mut intents_table = self.txn.open_table(SESSION_INTENTS)?;

        for (seq, node) in graph.nodes.iter().enumerate() {
            let seq = seq as u64;
            let detail: Option<serde_json::Value> = if let Some(s) = node.detail.as_deref() {
                match serde_json::from_str(s) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        log::warn!(
                            "Warning: failed to parse provenance node detail JSON (node id={}, kind={}): {}",
                            node.id,
                            node.kind.label(),
                            e
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Write every node as a SessionEvent regardless of kind.
            //
            // `event_kind` and `record_type` are populated from the original
            // Sherpa/agent-trace `record_type` field stored in the detail JSON
            // (e.g. "intent", "todo_status", "phase_transition").  We fall back
            // to `node.kind.label()` only when no such field is present so that
            // non-Sherpa nodes still get a meaningful discriminant.
            let sherpa_record_type = detail
                .as_ref()
                .and_then(|d| d["record_type"].as_str())
                .map(|s| s.to_string());
            let event_kind = sherpa_record_type
                .clone()
                .unwrap_or_else(|| node.kind.label().to_string());
            let event = SessionEvent {
                seq,
                timestamp: format_timestamp_ms(node.timestamp),
                event_kind,
                place: None,
                transition: None,
                token_id: node.id.clone(),
                token_kind: match node.kind {
                    ProvenanceNodeKind::Goal => "turn".to_string(),
                    _ => "todo".to_string(),
                },
                token_data: node.detail.clone().unwrap_or_default(),
                record_type: Some(
                    sherpa_record_type.unwrap_or_else(|| node.kind.label().to_string()),
                ),
            };

            let event_key = encode_session_event_key(provenance_id, seq);
            let event_bytes = event.to_bytes();
            events_table.insert(&event_key, event_bytes.as_slice())?;

            // Extract structured data by node kind.
            match node.kind {
                ProvenanceNodeKind::Goal => {
                    if let Some(ref detail) = detail {
                        let intent = IntentEntry {
                            title: detail["intent_title"]
                                .as_str()
                                .unwrap_or(&node.summary)
                                .to_string(),
                            description: detail["intent_description"]
                                .as_str()
                                .unwrap_or("")
                                .to_string(),
                            turn_id: detail["intent_turn_id"].as_u64().unwrap_or(0) as u32,
                            model: detail["model"].as_str().unwrap_or("").to_string(),
                            session_id: detail["session_id"]
                                .as_str()
                                .unwrap_or(&graph.session_id)
                                .to_string(),
                            outcome: String::new(),
                            total_tokens: detail["turn_totals"]["total"].as_u64().unwrap_or(0),
                            total_cost_usd: detail["turn_totals"]["cost_usd"]
                                .as_f64()
                                .unwrap_or(0.0),
                        };
                        let intent_bytes = intent.to_bytes();
                        intents_table.insert(provenance_id, intent_bytes.as_slice())?;

                        // Extract phase timing from GoalDetail.phases
                        if let Some(phases) = detail["phases"].as_object() {
                            for (phase_name, phase_data) in phases {
                                let entry = PhaseTimingEntry {
                                    phase: phase_name.clone(),
                                    start_ts: None,
                                    end_ts: None,
                                    input_tokens: phase_data["input"].as_u64().unwrap_or(0),
                                    output_tokens: phase_data["output"].as_u64().unwrap_or(0),
                                    cost_usd: phase_data["cost_usd"].as_f64().unwrap_or(0.0),
                                };
                                let phase_key = encode_session_phase_key(provenance_id, phase_name);
                                let phase_bytes = entry.to_bytes();
                                phases_table.insert(&phase_key, phase_bytes.as_slice())?;
                            }
                        }
                    }
                }

                ProvenanceNodeKind::Commitment => {
                    if let Some(ref detail) = detail {
                        let todo_id = detail["todo_id"].as_str().unwrap_or(&node.id).to_string();

                        let snapshot = TodoSnapshot {
                            todo_id: todo_id.clone(),
                            content: detail["todo_content"]
                                .as_str()
                                .unwrap_or(&node.summary)
                                .to_string(),
                            owner: detail["contributor"].as_str().unwrap_or("ai").to_string(),
                            final_status: "completed".to_string(),
                            priority: detail["priority"].as_str().unwrap_or("").to_string(),
                            file: detail["file"].as_str().map(|s| s.to_string()),
                            start_line: detail["start_line"].as_u64().map(|n| n as u32),
                            end_line: detail["end_line"].as_u64().map(|n| n as u32),
                            started_at: None,
                            completed_at: None,
                        };
                        let todo_key = encode_session_todo_key(provenance_id, &todo_id);
                        let todo_bytes = snapshot.to_bytes();
                        todos_table.insert(&todo_key, todo_bytes.as_slice())?;
                    }
                }

                ProvenanceNodeKind::Verification => {
                    if let Some(ref detail) = detail {
                        // Update the intent entry with outcome and totals.
                        // Read existing bytes in a separate scope so the
                        // immutable borrow on `intents_table` is released
                        // before we call `.insert()`.
                        let existing_bytes: Option<Vec<u8>> = {
                            match intents_table.get(provenance_id) {
                                Ok(Some(guard)) => Some(guard.value().to_vec()),
                                _ => None,
                            }
                        };

                        if let Some(bytes) = existing_bytes {
                            if let Ok(mut intent) = IntentEntry::from_bytes(&bytes) {
                                intent.outcome = detail["outcome"]
                                    .as_str()
                                    .unwrap_or("completed")
                                    .to_string();
                                if let Some(t) = detail["turn_tokens_total"].as_u64() {
                                    intent.total_tokens = t;
                                }
                                if let Some(c) = detail["turn_cost_usd"].as_f64() {
                                    intent.total_cost_usd = c;
                                }
                                let intent_bytes = intent.to_bytes();
                                intents_table.insert(provenance_id, intent_bytes.as_slice())?;
                            }
                        }
                    }
                }

                ProvenanceNodeKind::Todo => {
                    if let Some(ref detail) = detail {
                        let todo_id = detail["todo_id"].as_str().unwrap_or(&node.id).to_string();

                        let snapshot = TodoSnapshot {
                            todo_id: todo_id.clone(),
                            content: detail["content"]
                                .as_str()
                                .unwrap_or(&node.summary)
                                .to_string(),
                            owner: detail["owner"].as_str().unwrap_or("ai").to_string(),
                            final_status: detail["status"]
                                .as_str()
                                .unwrap_or("pending")
                                .to_string(),
                            priority: detail["priority"].as_str().unwrap_or("").to_string(),
                            file: None,
                            start_line: None,
                            end_line: None,
                            started_at: None,
                            completed_at: None,
                        };
                        let todo_key = encode_session_todo_key(provenance_id, &todo_id);
                        let todo_bytes = snapshot.to_bytes();
                        todos_table.insert(&todo_key, todo_bytes.as_slice())?;
                    }
                }

                _ => {
                    // Exploration, Execution, Decision, etc. —
                    // already captured as SessionEvent above.
                }
            }
        }

        Ok(())
    }
}

/// Format a Unix epoch milliseconds timestamp to RFC-3339 string.
fn format_timestamp_ms(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("{}", epoch_ms))
}

mod graph;
mod tree;
mod view;

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

    fn register_provenance(&mut self, hash: &Hash) -> PristineResult<NodeId> {
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
            node_types.insert(id, node_type::PROVENANCE)?;
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

    fn open_or_create_view(&mut self, name: &str) -> PristineResult<ViewState> {
        // Check if view exists
        {
            let table = self.txn.open_table(VIEWS)?;
            let result = table.get(name)?;
            if let Some(value) = result {
                return deserialize_view_state(value.value());
            }
        }

        // Create new view (defaults to Shared, no parent for backward compat)
        let id = self.next_view_id.fetch_add(1, Ordering::SeqCst);
        let state = ViewState::new(id, name.to_string());

        // Save it
        {
            let mut table = self.txn.open_table(VIEWS)?;
            let bytes = serialize_view_state(&state);
            table.insert(name, bytes.as_slice())?;
        }

        Ok(state)
    }

    fn create_view(
        &mut self,
        name: &str,
        kind: ViewScope,
        parent: Option<u64>,
    ) -> PristineResult<ViewState> {
        // Check if view already exists
        {
            let table = self.txn.open_table(VIEWS)?;
            if table.get(name)?.is_some() {
                return Err(PristineError::ViewAlreadyExists {
                    name: name.to_string(),
                });
            }
        }

        // Validate parent exists if specified, and detect cycles
        if let Some(parent_id) = parent {
            let parent_view = ViewTxnT::get_view_by_id(self, parent_id)?.ok_or_else(|| {
                PristineError::ViewNotFound {
                    name: format!("parent view id={}", parent_id),
                }
            })?;

            // Cycle detection: walk the parent chain from the proposed parent
            // upward. If we ever encounter our own (not-yet-allocated) name,
            // there's a cycle. Since the view doesn't exist yet, we only need
            // to check that the parent chain terminates without revisiting
            // `parent_id` — which is guaranteed as long as the existing graph
            // is acyclic and we're adding a leaf.
            //
            // However, we also guard against the degenerate case where someone
            // passes parent == self (once IDs are known). Since we haven't
            // allocated an ID yet, the only risk is the parent chain itself
            // being cyclic (which would be a pre-existing bug). We do a bounded
            // walk as a safety check.
            let mut visited = std::collections::HashSet::new();
            visited.insert(parent_id);
            let mut cursor = parent_view.parent;
            while let Some(ancestor_id) = cursor {
                if !visited.insert(ancestor_id) {
                    // We've seen this ID before — cycle detected in existing chain
                    return Err(PristineError::ViewCycleDetected {
                        name: name.to_string(),
                        parent_name: parent_view.name.clone(),
                    });
                }
                match ViewTxnT::get_view_by_id(self, ancestor_id)? {
                    Some(ancestor) => cursor = ancestor.parent,
                    None => break, // Broken chain — parent doesn't exist (shouldn't happen)
                }
            }
        }

        // Allocate ID and create state
        let id = self.next_view_id.fetch_add(1, Ordering::SeqCst);
        let state = ViewState::with_scope(id, name.to_string(), kind, parent);

        // Save it
        {
            let mut table = self.txn.open_table(VIEWS)?;
            let bytes = serialize_view_state(&state);
            table.insert(name, bytes.as_slice())?;
        }

        Ok(state)
    }

    fn put_change(
        &mut self,
        view: &mut ViewState,
        change_id: NodeId,
        change_hash: &Hash,
    ) -> PristineResult<u64> {
        let seq = view.change_count;

        // Add to change log
        {
            let mut table = self.txn.open_table(VIEW_CHANGES)?;
            let key = encode_view_seq(view.id, seq);
            table.insert(&key, change_id.get())?;
        }

        // Add reverse mapping
        {
            let mut table = self.txn.open_table(REV_VIEW_CHANGES)?;
            let key = encode_view_seq(view.id, change_id.get());
            table.insert(&key, seq)?;
        }

        // Update merkle state
        view.state = view.state.next(change_hash);
        view.change_count += 1;

        // Store the merkle state at this sequence
        {
            let mut table = self.txn.open_table(TAGS)?;
            let key = encode_view_seq(view.id, seq);
            table.insert(&key, view.state.as_bytes())?;
        }

        // Store state -> sequence mapping
        {
            let mut table = self.txn.open_table(STATES)?;
            let key = encode_view_merkle(view.id, view.state.as_bytes());
            table.insert(&key, seq)?;
        }

        Ok(seq)
    }

    fn del_change(
        &mut self,
        view: &mut ViewState,
        change_id: NodeId,
        _change_hash: &Hash,
    ) -> PristineResult<Option<u64>> {
        // Find the sequence number for this change
        let seq = {
            let table = self.txn.open_table(REV_VIEW_CHANGES)?;
            let key = encode_view_seq(view.id, change_id.get());
            let result = table.get(&key)?;
            match result {
                Some(value) => {
                    let v = value.value();
                    drop(value);
                    v
                }
                None => return Ok(None), // Change not in this view
            }
        };

        // Remove from VIEW_CHANGES
        {
            let mut table = self.txn.open_table(VIEW_CHANGES)?;
            let key = encode_view_seq(view.id, seq);
            table.remove(&key)?;
        }

        // Remove from REV_VIEW_CHANGES
        {
            let mut table = self.txn.open_table(REV_VIEW_CHANGES)?;
            let key = encode_view_seq(view.id, change_id.get());
            table.remove(&key)?;
        }

        // Remove from TAGS (the merkle state at this sequence)
        {
            let mut table = self.txn.open_table(TAGS)?;
            let key = encode_view_seq(view.id, seq);
            table.remove(&key)?;
        }

        // Shift all subsequent changes down by 1
        // We need to update sequences from seq+1 to change_count-1
        let original_count = view.change_count;
        for s in (seq + 1)..original_count {
            // Get the change_id at this sequence
            let cid = {
                let table = self.txn.open_table(VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, s);
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
                let mut table = self.txn.open_table(VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, s);
                table.remove(&key)?;
            }

            // Insert at new sequence (s - 1)
            {
                let mut table = self.txn.open_table(VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, s - 1);
                table.insert(&key, cid.get())?;
            }

            // Update reverse mapping
            {
                let mut table = self.txn.open_table(REV_VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, cid.get());
                table.insert(&key, s - 1)?;
            }
        }

        // Decrement change count
        view.change_count -= 1;

        // Recompute merkle state from scratch
        view.state = Merkle::ZERO;
        for s in 0..view.change_count {
            let cid = {
                let table = self.txn.open_table(VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, s);
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

            view.state = view.state.next(&hash);

            // Update TAGS with the new merkle state at this sequence
            {
                let mut table = self.txn.open_table(TAGS)?;
                let key = encode_view_seq(view.id, s);
                table.insert(&key, view.state.as_bytes())?;
            }

            // Update STATES mapping
            {
                let mut table = self.txn.open_table(STATES)?;
                let key = encode_view_merkle(view.id, view.state.as_bytes());
                table.insert(&key, s)?;
            }
        }

        Ok(Some(seq))
    }

    fn reinsert_change(
        &mut self,
        view: &mut ViewState,
        change_id: NodeId,
        change_hash: &Hash,
        at_sequence: u64,
    ) -> PristineResult<()> {
        // Clamp sequence to valid range
        let insert_at = at_sequence.min(view.change_count);

        // Shift all changes from insert_at onwards up by 1
        // Work backwards to avoid overwriting
        for s in (insert_at..view.change_count).rev() {
            // Get the change_id at this sequence
            let cid = {
                let table = self.txn.open_table(VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, s);
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
                let mut table = self.txn.open_table(VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, s);
                table.remove(&key)?;
            }

            // Insert at new sequence (s + 1)
            {
                let mut table = self.txn.open_table(VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, s + 1);
                table.insert(&key, cid.get())?;
            }

            // Update reverse mapping
            {
                let mut table = self.txn.open_table(REV_VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, cid.get());
                table.insert(&key, s + 1)?;
            }
        }

        // Insert the new change at the specified position
        {
            let mut table = self.txn.open_table(VIEW_CHANGES)?;
            let key = encode_view_seq(view.id, insert_at);
            table.insert(&key, change_id.get())?;
        }

        // Add reverse mapping
        {
            let mut table = self.txn.open_table(REV_VIEW_CHANGES)?;
            let key = encode_view_seq(view.id, change_id.get());
            table.insert(&key, insert_at)?;
        }

        // Increment change count
        view.change_count += 1;

        // Recompute merkle state from scratch
        view.state = Merkle::ZERO;
        for s in 0..view.change_count {
            let cid = {
                let table = self.txn.open_table(VIEW_CHANGES)?;
                let key = encode_view_seq(view.id, s);
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

            view.state = view.state.next(&hash);

            // Update TAGS with the new merkle state at this sequence
            {
                let mut table = self.txn.open_table(TAGS)?;
                let key = encode_view_seq(view.id, s);
                table.insert(&key, view.state.as_bytes())?;
            }

            // Update STATES mapping
            {
                let mut table = self.txn.open_table(STATES)?;
                let key = encode_view_merkle(view.id, view.state.as_bytes());
                table.insert(&key, s)?;
            }
        }

        Ok(())
    }

    fn update_view(&mut self, view: &ViewState) -> PristineResult<()> {
        let mut table = self.txn.open_table(VIEWS)?;
        let bytes = serialize_view_state(view);
        table.insert(view.name.as_str(), bytes.as_slice())?;
        Ok(())
    }

    fn del_view(&mut self, view: &ViewState) -> PristineResult<()> {
        // Guard: Shared views cannot be deleted (they own global GRAPH edges).
        if view.kind.is_shared() {
            return Err(PristineError::CannotDeleteSharedView {
                name: view.name.clone(),
            });
        }

        // Guard: Check for child views that reference this view as parent.
        // Deleting a parent would leave children with a dangling parent pointer.
        let children = ViewTxnT::get_children_views(self, view.id)?;
        if !children.is_empty() {
            let child_names: Vec<String> = children.iter().map(|c| c.name.clone()).collect();
            return Err(PristineError::ViewHasChildren {
                name: view.name.clone(),
                children: child_names,
            });
        }

        // Remove from VIEWS table
        {
            let mut table = self.txn.open_table(VIEWS)?;
            table.remove(view.name.as_str())?;
        }

        // Remove all change log entries for this view
        {
            let mut table = self.txn.open_table(VIEW_CHANGES)?;
            for seq in 0..view.change_count {
                let key = encode_view_seq(view.id, seq);
                table.remove(&key)?;
            }
        }

        // Remove all reverse change log entries
        {
            let mut rev_table = self.txn.open_table(REV_VIEW_CHANGES)?;
            let table = self.txn.open_table(VIEW_CHANGES)?;
            for seq in 0..view.change_count {
                let key = encode_view_seq(view.id, seq);
                if let Some(change_id) = table.get(&key)? {
                    let rev_key = encode_view_seq(view.id, change_id.value());
                    rev_table.remove(&rev_key)?;
                }
            }
        }

        // Remove all state/sequence mappings from STATES table
        {
            let mut table = self.txn.open_table(STATES)?;
            let tags_table = self.txn.open_table(TAGS)?;
            for seq in 0..view.change_count {
                let key = encode_view_seq(view.id, seq);
                if let Some(merkle_bytes) = tags_table.get(&key)? {
                    let merkle = merkle_bytes.value();
                    let state_key = encode_view_merkle(view.id, merkle);
                    table.remove(&state_key)?;
                }
            }
        }

        // Remove all tag entries from TAGS table
        {
            let mut table = self.txn.open_table(TAGS)?;
            for seq in 0..view.change_count {
                let key = encode_view_seq(view.id, seq);
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
            removed.map(|value| Inode::new(value.value()))
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

    fn get_deps(&self, change_id: NodeId) -> PristineResult<Vec<NodeId>> {
        let table = self.txn.open_multimap_table(DEPS)?;
        let mut result = Vec::new();
        let iter = table.get(change_id.get())?;
        for item in iter {
            let value = item?;
            result.push(NodeId::new(value.value()));
        }
        Ok(result)
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
                    results.push(Err(PristineError::Storage(Box::new(e))));
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
                    results.push(Err(PristineError::Storage(Box::new(e))));
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

    fn put_crdt_vertex_branch(
        &mut self,
        vertex_key: &[u8; 24],
        branch_key: &[u8; 12],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_table(VERTEX_BRANCH)?;
        table.insert(vertex_key, branch_key)?;
        Ok(())
    }

    fn get_crdt_vertex_branch(
        &mut self,
        vertex_key: &[u8; 24],
    ) -> PristineResult<Option<crate::crdt::BranchId>> {
        let table = self.txn.open_table(VERTEX_BRANCH)?;
        let result = table.get(vertex_key)?;
        match result {
            Some(value) => {
                let bytes: [u8; 12] = *value.value();
                drop(value);
                Ok(Some(decode_branch_id(&bytes)))
            }
            None => Ok(None),
        }
    }

    fn put_inodes(&mut self, inode: u64, pos: &Position<NodeId>) -> PristineResult<()> {
        let mut inodes_table = self.txn.open_table(INODES)?;
        let mut rev_inodes_table = self.txn.open_table(REV_INODES)?;

        let pos_bytes = encode_position(pos.change.get(), pos.pos.get());

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
