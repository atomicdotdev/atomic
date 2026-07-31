//! Session-ledger taxonomy — the single place where session-ledger rows are
//! arranged into the knowledge graph.
//!
//! Model (ATOM-16): the ontology (TBox, [`crate::pristine::ontology`]) types
//! the ledger's columns; this module is the ABox organization — the node-ID
//! minting, `rdf:type` assertions, and containment/derivation chains that
//! arrange session-ledger instances into the KG. The VCS graph stays
//! canonical and untouched: these triples classify over it, never alter it.
//!
//! Taxonomy rules (all of them, nowhere else):
//!
//! - Node IDs: `session:{id}`, `session:{id}/turn:{n}`, `provenance:{hash}`,
//!   `change:{12-char-truncated}` — the last joins the enrichment convention;
//!   the full hash rides in edge metadata for precision.
//! - Every relation uses an ontology predicate — no ad-hoc edge kinds.
//! - `rdf:type` is asserted in node metadata (`rdf_type` key, full URI);
//!   `KgNode.kind` stays a short searchable category like the rest of the KG.
//! - Emission is idempotent (upsert semantics) and rides the caller's write
//!   transaction; it never opens a second one.

use crate::change::session::{SessionRecord, SessionTurn};
use crate::pristine::error::PristineResult;
use crate::pristine::ontology::{edge_kind, entity_type, predicate};
use crate::pristine::traits::{KgMutTxnT, KgTxnT};
use crate::pristine::vault::{KgEdge, KgNode};
use crate::types::{Base32, Hash};

use super::WriteTxn;

/// Node id for a session ledger.
pub(crate) fn session_node_id(session_id: &str) -> String {
    format!("session:{}", session_id)
}

/// Node id for one turn row in a session ledger.
pub(crate) fn turn_node_id(session_id: &str, turn_number: u32) -> String {
    format!("session:{}/turn:{}", session_id, turn_number)
}

/// Node id for a provenance graph.
pub(crate) fn provenance_node_id(hash: &Hash) -> String {
    format!("provenance:{}", hash.to_base32())
}

/// Node id for a change, using the enrichment convention (12-char truncated).
pub(crate) fn change_node_id(hash: &Hash) -> String {
    let b32 = hash.to_base32();
    format!("change:{}", &b32[..12.min(b32.len())])
}

/// Node id for a todo. Agent-provided IDs are only stable within their
/// session, so qualify them with that session. Generated turn-local IDs are
/// already fully qualified and pass through unchanged.
pub(crate) fn todo_node_id(session_id: &str, todo_id: &str) -> String {
    if todo_id.starts_with(&format!("session:{session_id}/")) {
        todo_id.to_string()
    } else {
        format!("session:{session_id}/todo:{todo_id}")
    }
}

fn legacy_todo_node_id(todo_id: &str) -> String {
    if todo_id.starts_with("session:") {
        todo_id.to_string()
    } else {
        format!("todo:{todo_id}")
    }
}

fn is_session_owned_turn_edge(edge: &KgEdge, turn: &SessionTurn) -> bool {
    let turn_id = turn_node_id(&turn.session_id, turn.turn_number);
    let session_turn_prefix = format!("session:{}/turn:", turn.session_id);
    if edge.from_id == turn_id {
        return (edge.kind == predicate::EXPLAINED_BY
            && edge.to_id == provenance_node_id(&turn.provenance_hash))
            || (edge.kind == predicate::HAD_PLAN
                && turn.plan_id.as_ref().is_some_and(|plan_id| {
                    edge.to_id == format!("intent:{}", plan_id.to_uppercase())
                }))
            || (edge.kind == predicate::HAS_TODO
                && turn.todos.iter().any(|todo| {
                    edge.to_id == todo_node_id(&turn.session_id, &todo.id)
                        || edge.to_id == legacy_todo_node_id(&todo.id)
                }))
            || (edge.kind == predicate::GENERATED
                && turn
                    .change_hashes
                    .iter()
                    .any(|hash| edge.to_id == change_node_id(hash)))
            || (edge.kind == predicate::WAS_INFORMED_BY
                && edge.to_id.starts_with(&session_turn_prefix));
    }

    edge.to_id == turn_id
        && ((edge.kind == predicate::HAD_MEMBER
            && edge.from_id == session_node_id(&turn.session_id))
            || (edge.kind == predicate::WAS_INFORMED_BY
                && edge.from_id.starts_with(&session_turn_prefix)))
}

impl WriteTxn<'_> {
    /// Remove derived turn/todo KG state before a session ledger is
    /// re-numbered. Edges not owned by the session index are returned with
    /// their turn endpoints remapped so callers can restore them afterward.
    pub(crate) fn clear_session_turn_kg(
        &mut self,
        old_turns: &[SessionTurn],
        new_turns: &[SessionTurn],
    ) -> PristineResult<Vec<KgEdge>> {
        let mut todo_ids = std::collections::BTreeSet::new();
        let old_turn_ids: std::collections::BTreeSet<String> = old_turns
            .iter()
            .map(|turn| turn_node_id(&turn.session_id, turn.turn_number))
            .collect();
        let new_turn_ids: std::collections::HashMap<Hash, String> = new_turns
            .iter()
            .map(|turn| {
                (
                    turn.provenance_hash,
                    turn_node_id(&turn.session_id, turn.turn_number),
                )
            })
            .collect();
        let mut remapped_turn_ids = std::collections::HashMap::new();
        let mut preserved = std::collections::BTreeMap::new();

        for turn in old_turns {
            let old_turn_id = turn_node_id(&turn.session_id, turn.turn_number);
            if let Some(new_turn_id) = new_turn_ids.get(&turn.provenance_hash) {
                remapped_turn_ids.insert(old_turn_id.clone(), new_turn_id.clone());
            }
            for todo in &turn.todos {
                // Current namespaced form.
                todo_ids.insert(todo_node_id(&turn.session_id, &todo.id));
                // Pre-fix global form, for safe migration during a rewrite.
                todo_ids.insert(legacy_todo_node_id(&todo.id));
            }

            let edges = self
                .get_kg_edges_from(&old_turn_id)?
                .into_iter()
                .chain(self.get_kg_edges_to(&old_turn_id)?);
            for edge in edges {
                if !is_session_owned_turn_edge(&edge, turn) {
                    preserved.insert(
                        (edge.from_id.clone(), edge.to_id.clone(), edge.kind.clone()),
                        edge,
                    );
                }
            }
        }

        for turn_id in &old_turn_ids {
            self.del_kg_node(turn_id)?;
        }

        for todo_id in todo_ids {
            let is_session_todo = self
                .get_kg_node(&todo_id)?
                .is_some_and(|node| node.kind == "todo" && node.source == "session_ledger");
            if is_session_todo
                && self.get_kg_edges_from(&todo_id)?.is_empty()
                && self.get_kg_edges_to(&todo_id)?.is_empty()
            {
                self.del_kg_node(&todo_id)?;
            }
        }

        Ok(preserved
            .into_values()
            .map(|mut edge| {
                if let Some(new_id) = remapped_turn_ids.get(&edge.from_id) {
                    edge.from_id = new_id.clone();
                }
                if let Some(new_id) = remapped_turn_ids.get(&edge.to_id) {
                    edge.to_id = new_id.clone();
                }
                edge
            })
            .collect())
    }

    /// Check the derived nodes and edges covered by the session-ledger schema
    /// marker before an idempotent rebuild skips a full rewrite.
    pub(crate) fn session_turn_kg_is_current(
        &self,
        session_id: &str,
        turns: &[SessionTurn],
    ) -> PristineResult<bool> {
        let session_node = session_node_id(session_id);
        let is_current_session = self
            .get_kg_node(&session_node)?
            .is_some_and(|node| node.kind == "session" && node.source == "session_ledger");
        if !is_current_session {
            return Ok(false);
        }

        let session_edges = self.get_kg_edges_from(&session_node)?;
        let turn_numbers: std::collections::HashMap<Hash, u32> = turns
            .iter()
            .map(|turn| (turn.provenance_hash, turn.turn_number))
            .collect();
        let turn_prefix = format!("session:{session_id}/turn:");

        for turn in turns {
            if turn.session_id != session_id {
                return Ok(false);
            }

            let turn_id = turn_node_id(&turn.session_id, turn.turn_number);
            let is_current_turn = self
                .get_kg_node(&turn_id)?
                .is_some_and(|node| node.kind == "session_turn" && node.source == "session_ledger");
            if !is_current_turn
                || !session_edges
                    .iter()
                    .any(|edge| edge.kind == predicate::HAD_MEMBER && edge.to_id == turn_id)
            {
                return Ok(false);
            }

            let turn_edges = self.get_kg_edges_from(&turn_id)?;
            if !turn_edges.iter().any(|edge| {
                edge.kind == predicate::EXPLAINED_BY
                    && edge.to_id == provenance_node_id(&turn.provenance_hash)
            }) {
                return Ok(false);
            }

            let expected_previous = turn.previous_provenance.and_then(|hash| {
                turn_numbers
                    .get(&hash)
                    .map(|number| turn_node_id(session_id, *number))
            });
            let actual_previous: Vec<&str> = turn_edges
                .iter()
                .filter(|edge| {
                    edge.kind == predicate::WAS_INFORMED_BY && edge.to_id.starts_with(&turn_prefix)
                })
                .map(|edge| edge.to_id.as_str())
                .collect();
            match expected_previous {
                Some(expected)
                    if actual_previous.len() == 1 && actual_previous[0] == expected.as_str() => {}
                None if actual_previous.is_empty() => {}
                _ => return Ok(false),
            }

            if let Some(plan_id) = &turn.plan_id {
                let intent_id = format!("intent:{}", plan_id.to_uppercase());
                if !turn_edges
                    .iter()
                    .any(|edge| edge.kind == predicate::HAD_PLAN && edge.to_id == intent_id)
                    || !session_edges
                        .iter()
                        .any(|edge| edge.kind == predicate::HAD_PLAN && edge.to_id == intent_id)
                {
                    return Ok(false);
                }
            }

            for todo in &turn.todos {
                let current_id = todo_node_id(&turn.session_id, &todo.id);
                let is_current_todo = self
                    .get_kg_node(&current_id)?
                    .is_some_and(|node| node.kind == "todo" && node.source == "session_ledger");
                if !is_current_todo
                    || !turn_edges
                        .iter()
                        .any(|edge| edge.kind == predicate::HAS_TODO && edge.to_id == current_id)
                {
                    return Ok(false);
                }

                let legacy_id = legacy_todo_node_id(&todo.id);
                if legacy_id != current_id
                    && self
                        .get_kg_node(&legacy_id)?
                        .is_some_and(|node| node.kind == "todo" && node.source == "session_ledger")
                {
                    return Ok(false);
                }
            }

            for change_hash in &turn.change_hashes {
                let change_id = change_node_id(change_hash);
                if !turn_edges
                    .iter()
                    .any(|edge| edge.kind == predicate::GENERATED && edge.to_id == change_id)
                {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    /// Upsert the session node with its current lifecycle metadata.
    pub(crate) fn emit_session_node(&mut self, record: &SessionRecord) -> PristineResult<()> {
        let node = KgNode::new(
            session_node_id(&record.session_id),
            "session",
            &record.session_id,
            "session_ledger",
        )
        .with_metadata(serde_json::json!({
            "rdf_type": entity_type::SESSION,
            "view": record.view_name,
            "parent_view": record.parent_view,
            "status": if record.ended_at.is_some() { "ended" } else { "active" },
            "turn_count": record.turn_count,
            "json_path": record.json_path,
        }));
        self.upsert_kg_node(&node)?;

        // session ON_VIEW view — shares the enrichment's view vocabulary so
        // `neighbors view:X` surfaces sessions alongside changes and files.
        if let Some(view) = &record.view_name {
            self.upsert_kg_edge(&KgEdge::new(
                session_node_id(&record.session_id),
                format!("view:{}", view),
                edge_kind::ON_VIEW,
            ))?;
        }
        Ok(())
    }

    /// Emit one turn row's nodes and edges.
    ///
    /// Triples (all in the caller's transaction):
    /// - `session:{id}` (upserted with latest lifecycle metadata)
    /// - `provenance:{hash}` typed `vault:ProvenanceGraph`
    /// - `session:{id}/turn:{n}` typed `vault:SessionTurn`
    /// - session `prov:hadMember` turn
    /// - turn `vault:explainedBy` provenance
    /// - turn `prov:generated` change (per explained change; full hash in edge metadata)
    /// - turn `prov:wasInformedBy` the turn named by `previous_provenance`
    pub(crate) fn emit_turn_kg(
        &mut self,
        record: &SessionRecord,
        turn: &SessionTurn,
        previous_turn_number: Option<u32>,
    ) -> PristineResult<()> {
        self.emit_session_node(record)?;

        let prov_id = provenance_node_id(&turn.provenance_hash);
        let prov_node = KgNode::new(
            &prov_id,
            "provenance",
            &turn.provenance_hash.to_base32()[..12],
            "session_ledger",
        )
        .with_metadata(serde_json::json!({
            "rdf_type": entity_type::PROVENANCE,
            "session_id": turn.session_id,
            "timestamp": turn.timestamp,
        }));
        self.upsert_kg_node(&prov_node)?;

        let turn_id = turn_node_id(&turn.session_id, turn.turn_number);
        let turn_node = KgNode::new(
            &turn_id,
            "session_turn",
            format!("turn {}", turn.turn_number),
            "session_ledger",
        )
        .with_metadata(serde_json::json!({
            "rdf_type": entity_type::SESSION_TURN,
            "turn_number": turn.turn_number,
            "timestamp": turn.timestamp,
            "goal": turn.goal,
        }));
        let turn_node = match &turn.goal {
            Some(goal) => turn_node.with_summary(goal),
            None => turn_node,
        };
        self.upsert_kg_node(&turn_node)?;

        self.upsert_kg_edge(&KgEdge::new(
            session_node_id(&turn.session_id),
            &turn_id,
            predicate::HAD_MEMBER,
        ))?;

        self.upsert_kg_edge(&KgEdge::new(&turn_id, &prov_id, predicate::EXPLAINED_BY))?;

        if let Some(plan_id) = &turn.plan_id {
            let intent_id = format!("intent:{}", plan_id.to_uppercase());
            // The managed plan governs both the session and this concrete turn.
            self.upsert_kg_edge(&KgEdge::new(
                session_node_id(&turn.session_id),
                &intent_id,
                predicate::HAD_PLAN,
            ))?;
            self.upsert_kg_edge(&KgEdge::new(&turn_id, &intent_id, predicate::HAD_PLAN))?;
        }

        for todo in &turn.todos {
            let todo_id = todo_node_id(&turn.session_id, &todo.id);
            let todo_node = KgNode::new(&todo_id, "todo", &todo.content, "session_ledger")
                .with_summary(&todo.content)
                .with_metadata(serde_json::json!({
                    "rdf_type": entity_type::TODO,
                    "todo_id": todo.id,
                    "status": todo.status,
                    "priority": todo.priority,
                    "session_id": turn.session_id,
                    "turn_number": turn.turn_number,
                }));
            self.upsert_kg_node(&todo_node)?;
            self.upsert_kg_edge(
                &KgEdge::new(&turn_id, &todo_id, predicate::HAS_TODO).with_metadata(
                    serde_json::json!({
                        "status": todo.status,
                        "priority": todo.priority,
                    }),
                ),
            )?;
        }

        for change_hash in &turn.change_hashes {
            self.upsert_kg_edge(
                &KgEdge::new(&turn_id, change_node_id(change_hash), predicate::GENERATED)
                    .with_metadata(serde_json::json!({
                        "change_hash": change_hash.to_base32(),
                    })),
            )?;
        }

        if let Some(previous_turn_number) = previous_turn_number {
            self.upsert_kg_edge(&KgEdge::new(
                &turn_id,
                turn_node_id(&turn.session_id, previous_turn_number),
                predicate::WAS_INFORMED_BY,
            ))?;
        }

        Ok(())
    }

    /// Emit the fork-derivation triple: child session `prov:wasDerivedFrom`
    /// parent session, with the fork boundary in edge metadata.
    ///
    /// Public because the fork boundary is decided at the repository layer
    /// (`Repository::fork_session`); the turn/session emission stays internal
    /// to the index paths.
    pub fn emit_session_fork_kg(
        &mut self,
        child_session_id: &str,
        parent_session_id: &str,
        fork_turn: u32,
        parent_manifest: &Hash,
    ) -> PristineResult<()> {
        self.upsert_kg_edge(
            &KgEdge::new(
                session_node_id(child_session_id),
                session_node_id(parent_session_id),
                predicate::WAS_DERIVED_FROM,
            )
            .with_metadata(serde_json::json!({
                "fork_turn": fork_turn,
                "parent_manifest": parent_manifest.to_base32(),
            })),
        )
    }
}
