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
use crate::pristine::traits::KgMutTxnT;
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

/// Node id for a todo. Upstream IDs are global todo nodes; generated
/// turn-local snapshot IDs are already fully qualified session IDs.
pub(crate) fn todo_node_id(todo_id: &str) -> String {
    if todo_id.starts_with("session:") {
        todo_id.to_string()
    } else {
        format!("todo:{}", todo_id)
    }
}

impl WriteTxn<'_> {
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
    /// - turn `prov:wasInformedBy` previous turn (when turn_number > 0)
    pub(crate) fn emit_turn_kg(
        &mut self,
        record: &SessionRecord,
        turn: &SessionTurn,
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
            let todo_id = todo_node_id(&todo.id);
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

        if turn.turn_number > 0 {
            self.upsert_kg_edge(&KgEdge::new(
                &turn_id,
                turn_node_id(&turn.session_id, turn.turn_number - 1),
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
