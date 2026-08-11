//! Write transaction implementation
//!
//! This module provides the `WriteTxn` struct which implements read-write
//! access to the pristine database.

use std::sync::atomic::{AtomicU64, Ordering};

use redb::{ReadableMultimapTable, ReadableTable, WriteTransaction};

use crate::crdt::tables::{
    decode_branch_id, decode_branch_value, decode_leaf_value, decode_trunk_value,
    decode_vertex_position, SerializedBranch, SerializedLeaf, SerializedTrunk, BRANCHES,
    BRANCH_AFTER, BRANCH_LEAVES, BRANCH_VERTEX, INODE_TRUNK, LEAVES, PATH_TRUNK, TRUNKS,
    TRUNK_BRANCHES, VERTEX_BRANCH,
};

use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Merkle, NodeId, Position,
    SerializedGraphEdge,
};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::*;
use crate::pristine::traits::{
    FileIndexEntry, FileIndexMetadata, GraphTxnT, KgMutTxnT, MutTxnT, StoredConflict, TreeTxnT,
    ViewScope, ViewState, ViewTxnT,
};

use super::helpers::{
    deserialize_conflicts, deserialize_edge, deserialize_view_state, serialize_conflicts,
    serialize_edge, serialize_view_state, AdjIterator,
};

const SESSION_LEDGER_SCHEMA_VERSION: u32 = 1;

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

    /// Populate the session tables from a provenance graph.
    ///
    /// Called during `save_provenance_graph` for provenance produced by
    /// **any** agent — Sherpa, Claude Code, OpenCode, the generic
    /// `atomic-agent`, etc. Sherpa graphs (`profile ==
    /// "sherpa-trace/1.0.0"`) carry richer structured `detail` JSON
    /// (intent/todo/phase/verification), so they populate the intent and
    /// phase tables more fully; graphs from other agents still record every
    /// node as a `SessionEvent` and contribute todos/verification wherever
    /// their nodes and `detail` fields allow. Every extraction below reads
    /// `detail` defensively with fallbacks, so a missing Sherpa-specific key
    /// simply yields a sensible default rather than skipping the graph.
    ///
    /// Best-effort: parse errors on individual nodes are logged and skipped.
    pub fn populate_session_tables(
        &mut self,
        provenance_id: u64,
        graph: &crate::change::ProvenanceGraph,
    ) -> PristineResult<()> {
        use crate::change::provenance_graph::ProvenanceNodeKind;
        use crate::change::session::*;

        // No profile gate: provenance from any agent is indexed into the
        // session tables. The per-node extraction below is profile-agnostic
        // and falls back to node summary / kind labels when the richer
        // Sherpa-specific `detail` keys are absent.

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

    fn load_session_record_for_write(
        &self,
        session_id: &str,
    ) -> PristineResult<Option<crate::change::session::SessionRecord>> {
        use crate::change::session::SessionRecord;

        let sessions = self.txn.open_table(SESSIONS)?;
        let existing = sessions.get(session_id)?;
        match existing {
            Some(value) => SessionRecord::from_bytes(value.value())
                .map(Some)
                .map_err(|e| PristineError::Serialization {
                    message: format!("session record decode: {}", e),
                }),
            None => Ok(None),
        }
    }

    fn load_session_turns_for_write(
        &self,
        session_id: &str,
    ) -> PristineResult<Vec<crate::change::session::SessionTurn>> {
        use crate::change::session::{
            encode_session_turn_key, session_turn_namespace, SessionTurn,
        };

        let turns = self.txn.open_table(SESSION_TURNS)?;
        let namespace = session_turn_namespace(session_id);
        let scan_start = encode_session_turn_key(namespace, 0);
        let scan_end = encode_session_turn_key(namespace, u32::MAX);
        let mut result = Vec::new();

        for row in turns.range::<&[u8; 40]>(&scan_start..=&scan_end)? {
            let (_key, value) = row?;
            let turn = SessionTurn::from_bytes(value.value()).map_err(|e| {
                PristineError::Serialization {
                    message: format!("session turn decode: {}", e),
                }
            })?;
            if turn.session_id == session_id {
                result.push(turn);
            }
        }
        result.sort_by_key(|turn| turn.turn_number);
        Ok(result)
    }

    fn session_ledger_schema_key(session_id: &str) -> String {
        format!(
            "session_ledger_schema:{}",
            blake3::hash(session_id.as_bytes()).to_hex()
        )
    }

    fn has_current_session_ledger_schema(&self, session_id: &str) -> PristineResult<bool> {
        let metadata = self.txn.open_table(KG_INDEX_META)?;
        let key = Self::session_ledger_schema_key(session_id);
        let is_current = metadata
            .get(key.as_str())?
            .is_some_and(|value| value.value() >= SESSION_LEDGER_SCHEMA_VERSION);
        Ok(is_current)
    }

    fn mark_current_session_ledger_schema(&mut self, session_id: &str) -> PristineResult<()> {
        let mut metadata = self.txn.open_table(KG_INDEX_META)?;
        let key = Self::session_ledger_schema_key(session_id);
        metadata.insert(key.as_str(), SESSION_LEDGER_SCHEMA_VERSION)?;
        Ok(())
    }

    /// Append a turn without touching prior rows when the existing session was
    /// already written with the current canonical schema.
    fn append_session_turn(
        &mut self,
        mut record: crate::change::session::SessionRecord,
        turn: crate::change::session::SessionTurn,
        existing_turns: &[crate::change::session::SessionTurn],
    ) -> PristineResult<()> {
        use crate::change::session::{encode_session_turn_key, session_turn_namespace};

        let key =
            encode_session_turn_key(session_turn_namespace(&record.session_id), turn.turn_number);
        {
            let mut turns = self.txn.open_table(SESSION_TURNS)?;
            let mut reverse = self.txn.open_table(SESSION_PROVENANCE)?;
            let bytes = turn.to_bytes();
            turns.insert(&key, bytes.as_slice())?;
            reverse.insert(turn.provenance_hash.as_bytes(), &key)?;
        }

        if record.first_provenance.is_none() {
            record.first_provenance = Some(turn.provenance_hash);
        }
        record.latest_provenance = Some(turn.provenance_hash);
        record.turn_count = turn.turn_number.saturating_add(1);
        record.started_at = record.started_at.min(turn.timestamp);
        {
            let mut sessions = self.txn.open_table(SESSIONS)?;
            let bytes = record.to_bytes();
            sessions.insert(record.session_id.as_str(), bytes.as_slice())?;
        }

        let previous_turn_number = turn.previous_provenance.and_then(|hash| {
            existing_turns
                .iter()
                .find(|existing| existing.provenance_hash == hash)
                .map(|existing| existing.turn_number)
                .or_else(|| (hash == turn.provenance_hash).then_some(turn.turn_number))
        });
        self.emit_turn_kg(&record, &turn, previous_turn_number)
    }

    fn index_session_turn_candidate(
        &mut self,
        record: crate::change::session::SessionRecord,
        existing_turns: Vec<crate::change::session::SessionTurn>,
        mut candidate: crate::change::session::SessionTurn,
    ) -> PristineResult<()> {
        use crate::change::session::canonicalize_session_turns;

        let current_schema = self.has_current_session_ledger_schema(&record.session_id)?;
        candidate.turn_number = existing_turns.len() as u32;

        // The common chained append cannot sort before an existing turn:
        // causality keeps it blocked until the current last turn is emitted.
        let chained_append = existing_turns.is_empty()
            || candidate.previous_provenance
                == existing_turns.last().map(|turn| turn.provenance_hash);
        if current_schema && chained_append {
            return self.append_session_turn(record, candidate, &existing_turns);
        }

        let mut all_turns = existing_turns.clone();
        all_turns.push(candidate);
        let canonical = canonicalize_session_turns(all_turns);

        // Independent roots may also sort at the end. Keep the append-only
        // write path when canonicalization leaves every prior row untouched.
        if current_schema
            && canonical.get(..existing_turns.len()) == Some(existing_turns.as_slice())
        {
            if let Some(last) = canonical.last() {
                if !existing_turns
                    .iter()
                    .any(|turn| turn.provenance_hash == last.provenance_hash)
                {
                    return self.append_session_turn(record, last.clone(), &existing_turns);
                }
            }
        }

        self.rewrite_session_turns(record, canonical)
    }

    /// Rewrite one session's derived index from its complete immutable turn
    /// set. This keeps local keys, manifest order, lifecycle pointers, and KG
    /// turn numbers consistent even when provenance arrives out of order.
    fn rewrite_session_turns(
        &mut self,
        mut record: crate::change::session::SessionRecord,
        turns: Vec<crate::change::session::SessionTurn>,
    ) -> PristineResult<()> {
        use crate::change::session::{
            canonicalize_session_turns, encode_session_turn_key, session_turn_namespace,
        };

        let old_turns = self.load_session_turns_for_write(&record.session_id)?;
        let turns = canonicalize_session_turns(turns);
        let preserved_edges = self.clear_session_turn_kg(&old_turns, &turns)?;

        let namespace = session_turn_namespace(&record.session_id);
        let scan_start = encode_session_turn_key(namespace, 0);
        let scan_end = encode_session_turn_key(namespace, u32::MAX);
        let existing_keys = {
            let table = self.txn.open_table(SESSION_TURNS)?;
            let mut keys = Vec::new();
            for row in table.range::<&[u8; 40]>(&scan_start..=&scan_end)? {
                let (key, _value) = row?;
                keys.push(*key.value());
            }
            keys
        };
        {
            let mut table = self.txn.open_table(SESSION_TURNS)?;
            for key in existing_keys {
                table.remove(&key)?;
            }
        }
        {
            let mut table = self.txn.open_table(SESSION_TURNS)?;
            let mut reverse = self.txn.open_table(SESSION_PROVENANCE)?;
            for turn in &turns {
                let key = encode_session_turn_key(namespace, turn.turn_number);
                let bytes = turn.to_bytes();
                table.insert(&key, bytes.as_slice())?;
                reverse.insert(turn.provenance_hash.as_bytes(), &key)?;
            }
        }

        record.first_provenance = turns.first().map(|turn| turn.provenance_hash);
        record.latest_provenance = turns.last().map(|turn| turn.provenance_hash);
        record.turn_count = turns.len() as u32;
        if let Some(first) = turns.first() {
            record.started_at = record.started_at.min(first.timestamp);
        }
        {
            let mut sessions = self.txn.open_table(SESSIONS)?;
            let bytes = record.to_bytes();
            sessions.insert(record.session_id.as_str(), bytes.as_slice())?;
        }

        let turn_numbers: std::collections::HashMap<Hash, u32> = turns
            .iter()
            .map(|turn| (turn.provenance_hash, turn.turn_number))
            .collect();
        if turns.is_empty() {
            self.emit_session_node(&record)?;
        }
        for turn in &turns {
            let previous_turn_number = turn
                .previous_provenance
                .and_then(|hash| turn_numbers.get(&hash).copied());
            self.emit_turn_kg(&record, turn, previous_turn_number)?;
        }
        for edge in preserved_edges {
            self.upsert_kg_edge(&edge)?;
        }
        self.mark_current_session_ledger_schema(&record.session_id)?;

        Ok(())
    }

    /// Re-derive a stored session's canonical order and KG without adding a
    /// turn. Used by rebuild to repair indexes created by older versions.
    pub fn normalize_session_turn_order(&mut self, session_id: &str) -> PristineResult<()> {
        let Some(record) = self.load_session_record_for_write(session_id)? else {
            return Ok(());
        };
        let turns = self.load_session_turns_for_write(session_id)?;
        let canonical = crate::change::session::canonicalize_session_turns(turns.clone());
        if self.has_current_session_ledger_schema(session_id)?
            && canonical == turns
            && self.session_turn_kg_is_current(session_id, &turns)?
        {
            return Ok(());
        }
        self.rewrite_session_turns(record, canonical)
    }

    /// Index an immutable provenance graph as the next turn of its session.
    ///
    /// The session ID is the portable external identity. The turn key uses the
    /// full BLAKE3 namespace only for local ordering; the serialized value
    /// retains the original session ID so the index is self-describing.
    pub fn index_session_turn(
        &mut self,
        session_id: &str,
        json_path: &str,
        provenance_hash: &Hash,
        graph: &crate::change::ProvenanceGraph,
    ) -> PristineResult<()> {
        use crate::change::session::{SessionRecord, SessionTurn};

        // Idempotency is session-local because forked sessions legitimately
        // share provenance hashes.
        let turns = self.load_session_turns_for_write(session_id)?;
        if turns
            .iter()
            .any(|turn| turn.provenance_hash == *provenance_hash)
        {
            return Ok(());
        }

        let mut record = self
            .load_session_record_for_write(session_id)?
            .unwrap_or_else(|| SessionRecord {
                session_id: session_id.to_string(),
                json_path: json_path.to_string(),
                view_name: None,
                parent_view: None,
                first_provenance: None,
                latest_provenance: None,
                turn_count: 0,
                started_at: graph.timestamp,
                ended_at: None,
            });

        if record.json_path.is_empty() {
            record.json_path = json_path.to_string();
        }

        let candidate = SessionTurn {
            session_id: session_id.to_string(),
            // Canonical numbering is assigned from the complete turn set.
            turn_number: 0,
            goal: graph
                .nodes
                .iter()
                .find(|node| {
                    matches!(
                        node.kind,
                        crate::change::provenance_graph::ProvenanceNodeKind::Goal
                    )
                })
                .map(|node| node.summary.clone()),
            provenance_hash: *provenance_hash,
            change_hashes: graph.changes_explained.clone(),
            previous_provenance: graph.previous,
            timestamp: graph.timestamp,
            plan_id: graph.plan_id.clone(),
            todos: graph.todos.clone(),
        };

        self.index_session_turn_candidate(record, turns, candidate)
    }

    /// Index an inherited turn row for a forked session.
    ///
    /// The immutable turn data comes from the parent; the child derives its
    /// own canonical numbering from the complete inherited prefix.
    pub fn index_inherited_turn(
        &mut self,
        child_session_id: &str,
        json_path: &str,
        turn: &crate::change::session::SessionTurn,
    ) -> PristineResult<()> {
        use crate::change::session::SessionRecord;

        let turns = self.load_session_turns_for_write(child_session_id)?;
        if turns
            .iter()
            .any(|existing| existing.provenance_hash == turn.provenance_hash)
        {
            return Ok(());
        }

        let mut record = self
            .load_session_record_for_write(child_session_id)?
            .unwrap_or_else(|| SessionRecord {
                session_id: child_session_id.to_string(),
                json_path: json_path.to_string(),
                view_name: None,
                parent_view: None,
                first_provenance: None,
                latest_provenance: None,
                turn_count: 0,
                started_at: turn.timestamp,
                ended_at: None,
            });

        if record.json_path.is_empty() {
            record.json_path = json_path.to_string();
        }

        let mut inherited = turn.clone();
        inherited.session_id = child_session_id.to_string();
        inherited.turn_number = 0;

        self.index_session_turn_candidate(record, turns, inherited)
    }

    /// Create an empty session record (e.g., a fork with no inherited turns).
    pub fn index_empty_session(
        &mut self,
        session_id: &str,
        json_path: &str,
        view_name: Option<String>,
        parent_view: Option<String>,
    ) -> PristineResult<()> {
        use crate::change::session::SessionRecord;

        let mut sessions = self.txn.open_table(SESSIONS)?;
        if sessions.get(session_id)?.is_some() {
            return Ok(());
        }
        let record = SessionRecord {
            session_id: session_id.to_string(),
            json_path: json_path.to_string(),
            view_name,
            parent_view,
            first_provenance: None,
            latest_provenance: None,
            turn_count: 0,
            started_at: chrono::Utc::now().timestamp(),
            ended_at: None,
        };
        let bytes = record.to_bytes();
        sessions.insert(session_id, bytes.as_slice())?;
        drop(sessions);
        self.emit_session_node(&record)
    }

    /// Reconcile lifecycle metadata for a session record.
    ///
    /// Called from agent session start/end hooks. Creates the record if the
    /// session has no ledger entries yet; otherwise updates only the fields
    /// supplied, preserving ledger data (provenance links, turn count).
    /// `ended_at: None` on a re-entered (resumed) session clears a prior end.
    pub fn upsert_session_lifecycle(
        &mut self,
        session_id: &str,
        json_path: &str,
        view_name: Option<String>,
        parent_view: Option<String>,
        ended_at: Option<i64>,
    ) -> PristineResult<()> {
        use crate::change::session::SessionRecord;

        let mut sessions = self.txn.open_table(SESSIONS)?;
        let mut record = match sessions.get(session_id)? {
            Some(value) => SessionRecord::from_bytes(value.value()).map_err(|e| {
                PristineError::Serialization {
                    message: format!("session record decode: {}", e),
                }
            })?,
            None => SessionRecord {
                session_id: session_id.to_string(),
                json_path: json_path.to_string(),
                view_name: None,
                parent_view: None,
                first_provenance: None,
                latest_provenance: None,
                turn_count: 0,
                started_at: chrono::Utc::now().timestamp(),
                ended_at: None,
            },
        };

        if record.json_path.is_empty() {
            record.json_path = json_path.to_string();
        }
        if view_name.is_some() {
            record.view_name = view_name;
        }
        if parent_view.is_some() {
            record.parent_view = parent_view;
        }
        // Explicit assignment: Some(ts) ends the session, None clears a
        // stale end marker when a session is re-entered.
        record.ended_at = ended_at;

        let bytes = record.to_bytes();
        sessions.insert(session_id, bytes.as_slice())?;

        // Refresh the session node's lifecycle metadata (ATOM-16).
        drop(sessions);
        self.emit_session_node(&record)?;
        Ok(())
    }

    /// Store an immutable session manifest and advance its convenience head.
    pub fn save_session_manifest(
        &mut self,
        manifest: &crate::change::session::SessionManifest,
    ) -> PristineResult<Hash> {
        let hash = manifest.content_hash();
        let mut manifests = self.txn.open_table(SESSION_MANIFESTS)?;
        let mut heads = self.txn.open_table(SESSION_HEADS)?;
        let bytes = manifest.to_bytes();
        manifests.insert(hash.as_bytes(), bytes.as_slice())?;
        heads.insert(manifest.session_id.as_str(), hash.as_bytes())?;
        Ok(hash)
    }
}

/// Format a Unix epoch milliseconds timestamp to RFC-3339 string.
fn format_timestamp_ms(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("{}", epoch_ms))
}

mod embeddings;
mod graph;
mod session_kg;
mod tag;
mod tree;
mod triples;
mod vault;
mod view;

#[cfg(test)]
mod tests;

// Entity registration helper

impl<'a> WriteTxn<'a> {
    /// Register an entity in the identity tables.
    ///
    /// Creates INTERNAL (hash → id), EXTERNAL (id → hash), and NODE_TYPES (id → type)
    /// entries. Returns existing id if already registered.
    fn register_entity(&mut self, hash: &Hash, entity_type: u8) -> PristineResult<NodeId> {
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
            node_types.insert(id, entity_type)?;
        }

        Ok(node_id)
    }
}

// MutTxnT Implementation

impl<'a> MutTxnT for WriteTxn<'a> {
    fn register_change(&mut self, hash: &Hash) -> PristineResult<NodeId> {
        self.register_entity(hash, node_type::CHANGE)
    }

    fn register_tag(&mut self, hash: &Hash) -> PristineResult<NodeId> {
        self.register_entity(hash, node_type::TAG)
    }

    fn register_attestation(&mut self, hash: &Hash) -> PristineResult<NodeId> {
        self.register_entity(hash, node_type::ATTESTATION)
    }

    fn register_provenance(&mut self, hash: &Hash) -> PristineResult<NodeId> {
        self.register_entity(hash, node_type::PROVENANCE)
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
            let mut table = self.txn.open_table(MERKLE_CHAIN)?;
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

        // Remove from MERKLE_CHAIN (the merkle state at this sequence)
        {
            let mut table = self.txn.open_table(MERKLE_CHAIN)?;
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

            // Update MERKLE_CHAIN with the new merkle state at this sequence
            {
                let mut table = self.txn.open_table(MERKLE_CHAIN)?;
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

            // Update MERKLE_CHAIN with the new merkle state at this sequence
            {
                let mut table = self.txn.open_table(MERKLE_CHAIN)?;
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
            let merkle_table = self.txn.open_table(MERKLE_CHAIN)?;
            for seq in 0..view.change_count {
                let key = encode_view_seq(view.id, seq);
                if let Some(merkle_bytes) = merkle_table.get(&key)? {
                    let merkle = merkle_bytes.value();
                    let state_key = encode_view_merkle(view.id, merkle);
                    table.remove(&state_key)?;
                }
            }
        }

        // Remove all entries from MERKLE_CHAIN table
        {
            let mut table = self.txn.open_table(MERKLE_CHAIN)?;
            for seq in 0..view.change_count {
                let key = encode_view_seq(view.id, seq);
                table.remove(&key)?;
            }
        }

        // Remove all named tags for this view from TAG_RECORDS + TAG_NAME_INDEX
        use crate::pristine::traits::tag::TagMutTxnT;
        self.del_tags_for_view(&view.name)?;

        // Remove all persisted conflict state for this view.
        self.del_conflicts_prefix(view.id)?;

        Ok(())
    }

    fn put_conflicts(
        &mut self,
        view_id: u64,
        inode: u64,
        conflicts: &[StoredConflict],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_table(CONFLICTS)?;
        let key = encode_view_seq(view_id, inode);
        if conflicts.is_empty() {
            table.remove(&key)?;
        } else {
            let bytes = serialize_conflicts(conflicts)?;
            table.insert(&key, bytes.as_slice())?;
        }
        Ok(())
    }

    fn del_conflicts(&mut self, view_id: u64, inode: u64) -> PristineResult<()> {
        let mut table = self.txn.open_table(CONFLICTS)?;
        let key = encode_view_seq(view_id, inode);
        table.remove(&key)?;
        Ok(())
    }

    fn del_conflicts_prefix(&mut self, view_id: u64) -> PristineResult<()> {
        // Collect the keys in range first to avoid iterating while removing.
        let keys: Vec<[u8; 16]> = {
            let table = self.txn.open_table(CONFLICTS)?;
            let start = encode_view_seq(view_id, 0);
            let end = encode_view_seq(view_id, u64::MAX);
            let mut keys = Vec::new();
            for entry in table.range::<&[u8; 16]>(&start..=&end)? {
                let (key, _) = entry?;
                keys.push(*key.value());
            }
            keys
        };
        let mut table = self.txn.open_table(CONFLICTS)?;
        for key in &keys {
            table.remove(key)?;
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

    fn put_file_index(
        &mut self,
        path: &str,
        mtime_secs: i64,
        mtime_nanos: u32,
        file_size: u64,
        content_hash: &Hash,
    ) -> Result<(), PristineError> {
        let value = encode_file_index(mtime_secs, mtime_nanos, file_size, content_hash);
        let mut table = self.txn.open_table(FILE_INDEX)?;
        table.insert(path, &value)?;
        Ok(())
    }

    fn del_file_index(&mut self, path: &str) -> Result<(), PristineError> {
        let mut table = self.txn.open_table(FILE_INDEX)?;
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

    fn put_change_deps(&mut self, change_id: NodeId, deps: &[Hash]) -> PristineResult<()> {
        let existing: Vec<[u8; 32]> = {
            let table = self.txn.open_multimap_table(CHANGE_DEPS)?;
            let iter = table.get(change_id.get())?;
            let mut hashes = Vec::new();
            for item in iter {
                hashes.push(*item?.value());
            }
            hashes
        };

        if !existing.is_empty() {
            {
                let mut table = self.txn.open_multimap_table(CHANGE_DEPS)?;
                for dep in &existing {
                    table.remove(change_id.get(), dep)?;
                }
            }
            {
                let mut table = self.txn.open_multimap_table(REV_CHANGE_DEPS)?;
                for dep in &existing {
                    table.remove(dep, change_id.get())?;
                }
            }
        }

        {
            let mut table = self.txn.open_multimap_table(CHANGE_DEPS)?;
            for dep in deps {
                table.insert(change_id.get(), dep.as_bytes())?;
            }
        }
        {
            let mut table = self.txn.open_multimap_table(REV_CHANGE_DEPS)?;
            for dep in deps {
                table.insert(dep.as_bytes(), change_id.get())?;
            }
        }
        {
            let mut table = self.txn.open_table(CHANGE_DEPS_INDEXED)?;
            table.insert(change_id.get(), deps.len() as u64)?;
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

    // CRDT Table Write Operations
    //
    // CRDT *read* methods are implemented on the [`super::CrdtTxnT`] impl
    // below (which `MutTxnT` requires as a supertrait).

    fn put_crdt_trunk(&mut self, key: &[u8; 12], value: &[u8]) -> PristineResult<()> {
        let mut table = self.txn.open_table(TRUNKS)?;
        table.insert(key, value)?;
        Ok(())
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

    fn put_crdt_trunk_branch(
        &mut self,
        trunk_key: &[u8; 12],
        branch_key: &[u8; 12],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_multimap_table(TRUNK_BRANCHES)?;
        table.insert(trunk_key, branch_key)?;
        Ok(())
    }

    fn put_crdt_branch_after(
        &mut self,
        branch_key: &[u8; 12],
        after_key: &[u8; 12],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_table(BRANCH_AFTER)?;
        table.insert(branch_key, after_key)?;
        Ok(())
    }

    fn put_crdt_leaf(&mut self, key: &[u8; 12], value: &[u8; 22]) -> PristineResult<()> {
        let mut table = self.txn.open_table(LEAVES)?;
        table.insert(key, value)?;
        Ok(())
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

    fn put_crdt_branch_vertex(
        &mut self,
        branch_key: &[u8; 12],
        node_bytes: &[u8; 24],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_table(BRANCH_VERTEX)?;
        table.insert(branch_key, node_bytes)?;
        Ok(())
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

// CrdtTxnT (read accessors) implementation for WriteTxn.
//
// Both `WriteTransaction::open_table` and `ReadTransaction::open_table`
// take `&self` (per redb 2.6.x), so all CRDT *reads* fit a `&self` trait —
// which lets us share the same trait between read-only and read-write
// txns and avoids needing a write_txn just to inspect CRDT state.

impl crate::pristine::traits::CrdtTxnT for WriteTxn<'_> {
    fn get_crdt_trunk(&self, key: &[u8; 12]) -> PristineResult<Option<SerializedTrunk>> {
        let table = self.txn.open_table(TRUNKS)?;
        let result = match table.get(key)? {
            Some(guard) => {
                let bytes_copy: Vec<u8> = guard.value().to_vec();
                decode_trunk_value(&bytes_copy)
            }
            None => None,
        };
        Ok(result)
    }

    fn get_crdt_inode_trunk(&self, inode: u64) -> PristineResult<Option<[u8; 12]>> {
        let table = self.txn.open_table(INODE_TRUNK)?;
        let result = table.get(inode)?.map(|v| *v.value());
        Ok(result)
    }

    fn get_crdt_branch(&self, key: &[u8; 12]) -> PristineResult<Option<SerializedBranch>> {
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

    fn get_crdt_branch_after(&self, branch_key: &[u8; 12]) -> PristineResult<Option<[u8; 12]>> {
        let table = self.txn.open_table(BRANCH_AFTER)?;
        let result = table.get(branch_key)?.map(|v| *v.value());
        Ok(result)
    }

    fn get_crdt_leaf(&self, key: &[u8; 12]) -> PristineResult<Option<SerializedLeaf>> {
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

    fn get_trunk_by_path(&self, path: &str) -> PristineResult<Option<crate::crdt::TrunkId>> {
        use crate::crdt::tables::decode_trunk_id;
        let table = self.txn.open_table(PATH_TRUNK)?;
        let result = table.get(path)?;
        match result {
            Some(guard) => {
                let key: [u8; 12] = *guard.value();
                drop(guard);
                Ok(Some(decode_trunk_id(&key)))
            }
            None => Ok(None),
        }
    }

    fn iter_trunk_branches(
        &self,
        trunk_key: &[u8; 12],
    ) -> PristineResult<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>> {
        let table = self.txn.open_multimap_table(TRUNK_BRANCHES)?;
        let mut results: Vec<Result<[u8; 12], PristineError>> = Vec::new();
        let values = table.get(trunk_key)?;
        for value_result in values {
            match value_result {
                Ok(access) => results.push(Ok(*access.value())),
                Err(e) => results.push(Err(PristineError::Storage(Box::new(e)))),
            }
        }
        Ok(Box::new(results.into_iter()))
    }

    fn iter_branch_leaves(
        &self,
        branch_key: &[u8; 12],
    ) -> PristineResult<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>> {
        let table = self.txn.open_multimap_table(BRANCH_LEAVES)?;
        let mut results: Vec<Result<[u8; 12], PristineError>> = Vec::new();
        let values = table.get(branch_key)?;
        for value_result in values {
            match value_result {
                Ok(access) => results.push(Ok(*access.value())),
                Err(e) => results.push(Err(PristineError::Storage(Box::new(e)))),
            }
        }
        Ok(Box::new(results.into_iter()))
    }

    fn get_crdt_branch_vertex(
        &self,
        branch_key: &[u8; 12],
    ) -> PristineResult<Option<GraphNode<NodeId>>> {
        let table = self.txn.open_table(BRANCH_VERTEX)?;
        let result = table.get(branch_key)?;
        match result {
            Some(value) => {
                let bytes: [u8; 24] = *value.value();
                drop(value);
                Ok(Some(decode_vertex_position(&bytes)))
            }
            None => Ok(None),
        }
    }

    fn get_crdt_vertex_branch(
        &self,
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
}
