//! Offline projection of authoritative session-ledger rows. Class assertions
//! live outside neighborhood edges; view context never asserts membership.

use super::WriteTxn;
use crate::change::session::{SessionRecord, SessionTurn};
use crate::kg::{
    iri,
    ontology::{Class, Predicate},
    project_id::ProjectId,
};
use crate::pristine::vault::{KgEdge, KgNode};
use crate::pristine::{
    GraphTxnT, KgMutTxnT, KgTxnT, PristineError, PristineResult, VaultTxnT, ViewTxnT,
};
use crate::types::{Base32, Hash};
use redb::ReadableTable;

impl WriteTxn<'_> {
    pub fn kg_project(&self) -> PristineResult<ProjectId> {
        self.project_identity()?
            .ok_or(PristineError::MissingProjectIdentity)?
            .project_id()
            .map_err(|e| PristineError::Serialization {
                message: e.to_string(),
            })
    }

    /// Resolve a plan through authoritative manifest identity; a raw label is
    /// insufficient evidence to invent a canonical intent resource.
    pub fn kg_intent_id(&self, raw: &str) -> PristineResult<Option<String>> {
        let raw = raw.strip_prefix("urn:atomic:intent:").unwrap_or(raw);
        let manifest = self.get_vault_manifest()?;
        let summary = manifest.intents.iter().find(|(key, summary)| {
            key.eq_ignore_ascii_case(raw) || summary.uid.eq_ignore_ascii_case(raw)
        });
        Ok(summary.and_then(|(_, summary)| {
            iri::mint_intent(&self.kg_project().ok()?, &summary.uid.to_uppercase()).ok()
        }))
    }

    pub fn kg_finding(&mut self, code: &str, subject: &str, message: &str) -> PristineResult<()> {
        let id = iri::mint_finding(&self.kg_project()?, code, subject);
        self.upsert_kg_node(
            &KgNode::new(&id, Class::Finding, code, "findings")
                .with_summary(message)
                .with_metadata(serde_json::json!({"finding_code": code})),
        )?;
        self.upsert_kg_edge(&KgEdge::new(id, subject, Predicate::AboutResource))
    }

    pub fn kg_clear_finding(&mut self, code: &str, subject: &str) -> PristineResult<()> {
        self.del_kg_node(&iri::mint_finding(&self.kg_project()?, code, subject))?;
        Ok(())
    }

    pub(crate) fn clear_session_turn_kg(
        &mut self,
        old: &[SessionTurn],
        new: &[SessionTurn],
    ) -> PristineResult<Vec<KgEdge>> {
        if !self.kg_writes_enabled()? {
            return Ok(Vec::new());
        }
        let project = self.kg_project()?;
        let replacements: std::collections::HashMap<Hash, String> = new
            .iter()
            .map(|turn| {
                (
                    turn.provenance_hash,
                    iri::mint_turn(&project, &turn.session_id, turn.turn_number),
                )
            })
            .collect();
        let mut remap = std::collections::HashMap::new();
        let mut preserved = std::collections::BTreeMap::new();
        for turn in old {
            let id = iri::mint_turn(&project, &turn.session_id, turn.turn_number);
            if let Some(new_id) = replacements.get(&turn.provenance_hash) {
                remap.insert(id.clone(), new_id.clone());
            }
            for edge in self
                .get_kg_edges_from(&id)?
                .into_iter()
                .chain(self.get_kg_edges_to(&id)?)
            {
                if edge
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("projector"))
                    .and_then(|m| m.as_str())
                    != Some("session_ledger")
                {
                    preserved.insert((edge.from_id.clone(), edge.to_id.clone(), edge.kind), edge);
                }
            }
            self.del_kg_node(&id)?;
            for hash in &turn.change_hashes {
                let change = iri::mint_change(hash);
                for edge in self.get_kg_edges_from(&change)? {
                    if edge
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("turn_provenance"))
                        .and_then(|m| m.as_str())
                        == Some(turn.provenance_hash.to_base32().as_str())
                    {
                        self.del_kg_edge(&edge.from_id, &edge.to_id, &edge.kind)?;
                    }
                }
            }
        }
        Ok(preserved
            .into_values()
            .map(|mut edge| {
                if let Some(id) = remap.get(&edge.from_id) {
                    edge.from_id = id.clone();
                }
                if let Some(id) = remap.get(&edge.to_id) {
                    edge.to_id = id.clone();
                }
                edge
            })
            .collect())
    }

    pub(crate) fn session_turn_kg_is_current(
        &self,
        session: &str,
        turns: &[SessionTurn],
    ) -> PristineResult<bool> {
        if !self.kg_writes_enabled()? {
            return Ok(true);
        }
        let project = self.kg_project()?;
        if self
            .get_kg_node(&iri::mint_session(&project, session))?
            .is_none()
        {
            return Ok(false);
        }
        for turn in turns {
            let id = iri::mint_turn(&project, session, turn.turn_number);
            let Some(node) = self.get_kg_node(&id)? else {
                return Ok(false);
            };
            if !node.has_type(Class::SessionTurn)
                || node.has_type(Class::ManagedTurn) != turn.plan_id.is_some()
            {
                return Ok(false);
            }
            let edges = self.get_kg_edges_from(&id)?;
            for todo in &turn.todos {
                let todo = iri::mint_todo(&project, session, &todo.id);
                if self.get_kg_node(&todo)?.is_none()
                    || !edges
                        .iter()
                        .any(|edge| edge.kind == Predicate::HasTodo && edge.to_id == todo)
                {
                    return Ok(false);
                }
            }
            if let Some(previous) = turn.previous_provenance.and_then(|hash| {
                turns
                    .iter()
                    .find(|candidate| candidate.provenance_hash == hash)
            }) {
                let previous = iri::mint_turn(&project, session, previous.turn_number);
                if !edges
                    .iter()
                    .any(|edge| edge.kind == Predicate::ProvWasInformedBy && edge.to_id == previous)
                {
                    return Ok(false);
                }
            }
            for hash in &turn.change_hashes {
                if !edges.iter().any(|e| {
                    e.kind == Predicate::GeneratedChange && e.to_id == iri::mint_change(hash)
                }) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    pub(crate) fn emit_session_node(&mut self, record: &SessionRecord) -> PristineResult<()> {
        if !self.kg_writes_enabled()? {
            return Ok(());
        }
        let project = self.kg_project()?;
        let id = iri::mint_session(&project, &record.session_id);
        self.upsert_kg_node(&KgNode::new(&id, Class::Session, &record.session_id, "session_ledger")
            .with_metadata(serde_json::json!({"view": record.view_name, "parent_view": record.parent_view,
                "status": record.status.label(), "incomplete": record.status.incomplete(), "turn_count": record.turn_count,
                "json_path": record.json_path})))?;
        for edge in self.get_kg_edges_from(&id)? {
            if edge.kind == Predicate::SessionViewContext {
                self.del_kg_edge(&edge.from_id, &edge.to_id, &edge.kind)?;
            }
        }
        if let Some(view) = &record.view_name {
            let view_id = iri::mint_view(&project, view);
            if self.get_view(view)?.is_some() {
                if self.get_kg_node(&view_id)?.is_none() {
                    self.upsert_kg_node(&KgNode::new(&view_id, Class::View, view, "vcs"))?;
                }
                self.upsert_kg_edge(&KgEdge::new(&id, view_id, Predicate::SessionViewContext))?;
            }
        }
        Ok(())
    }

    pub(crate) fn emit_turn_kg(
        &mut self,
        record: &SessionRecord,
        turn: &SessionTurn,
        previous: Option<u32>,
    ) -> PristineResult<()> {
        if !self.kg_writes_enabled()? {
            return Ok(());
        }
        let project = self.kg_project()?;
        self.emit_session_node(record)?;
        let id = iri::mint_turn(&project, &turn.session_id, turn.turn_number);
        // Replace only this projector's facts. Independent workflow references
        // survive replay, while removed plans/todos/shortcuts cannot linger.
        for old in self.get_kg_edges_from(&id)? {
            if old
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("projector"))
                .and_then(|value| value.as_str())
                == Some("session_ledger")
            {
                self.del_kg_edge(&old.from_id, &old.to_id, &old.kind)?;
            }
        }
        for hash in &turn.change_hashes {
            for old in self.get_kg_edges_from(&iri::mint_change(hash))? {
                if old
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("turn_provenance"))
                    .and_then(|value| value.as_str())
                    == Some(turn.provenance_hash.to_base32().as_str())
                {
                    self.del_kg_edge(&old.from_id, &old.to_id, &old.kind)?;
                }
            }
        }
        self.kg_clear_finding("UnresolvedReference", &id)?;
        let created_at = chrono::DateTime::from_timestamp(turn.timestamp, 0)
            .ok_or_else(|| PristineError::Serialization {
                message: "session turn timestamp is outside the RDF dateTime range".into(),
            })?
            .to_rfc3339();
        let mut node = KgNode::new(&id, Class::SessionTurn, format!("turn {}", turn.turn_number), "session_ledger")
            .with_metadata(serde_json::json!({"turn_number": turn.turn_number, "created_at": created_at, "goal": turn.goal,
                "plan_id": turn.plan_id, "outcome": turn.outcome, "boundary_start": turn.boundary_start, "boundary_end": turn.boundary_end}));
        if turn.plan_id.is_some() {
            node = node.with_type(Class::ManagedTurn);
        }
        if let Some(goal) = &turn.goal {
            node = node.with_summary(goal);
        }
        self.upsert_kg_node(&node)?;
        let edge = |from: &str, to: &str, predicate| {
            KgEdge::new(from, to, predicate)
            .with_metadata(serde_json::json!({"projector": "session_ledger", "turn_provenance": turn.provenance_hash.to_base32()}))
        };
        self.upsert_kg_edge(&edge(
            &iri::mint_session(&project, &turn.session_id),
            &id,
            Predicate::HasTurn,
        ))?;
        let provenance = iri::mint_provenance(&turn.provenance_hash);
        let has_provenance = self.get_internal(&turn.provenance_hash)?.is_some();
        if has_provenance {
            self.upsert_kg_node(&KgNode::new(
                &provenance,
                Class::ProvenanceGraph,
                &turn.provenance_hash.to_base32()[..12],
                "session_ledger",
            ))?;
            self.upsert_kg_edge(&edge(&id, &provenance, Predicate::TurnExplainedBy))?;
        }
        let plan = match turn.plan_id.as_deref() {
            Some(raw) => self.kg_intent_id(raw)?,
            None => None,
        };
        if let Some(plan) = &plan {
            if self.get_kg_node(plan)?.is_some() {
                self.upsert_kg_edge(&edge(&id, plan, Predicate::HasPlan))?;
            } else {
                self.kg_finding(
                    "UnresolvedReference",
                    &id,
                    "The governing intent is not projected locally",
                )?;
            }
        } else if turn.plan_id.is_some() {
            self.kg_finding(
                "UnresolvedReference",
                &id,
                "The governing intent could not be resolved through the vault manifest",
            )?;
        }
        for todo in &turn.todos {
            let todo_id = iri::mint_todo(&project, &turn.session_id, &todo.id);
            self.upsert_kg_node(&KgNode::new(&todo_id, Class::Todo, &todo.content, "session_ledger")
                .with_summary(&todo.content).with_metadata(serde_json::json!({"todo_id": todo.id, "status": todo.status,
                    "priority": todo.priority, "session_id": turn.session_id, "turn_number": turn.turn_number})))?;
            self.upsert_kg_edge(&edge(&id, &todo_id, Predicate::HasTodo))?;
        }
        for hash in &turn.change_hashes {
            let change = iri::mint_change(hash);
            if self.get_kg_node(&change)?.is_none() {
                self.upsert_kg_node(
                    &KgNode::new(
                        &change,
                        Class::Change,
                        &hash.to_base32()[..12],
                        "session_ledger",
                    )
                    .with_metadata(serde_json::json!({"hash": hash.to_base32()})),
                )?;
            }
            self.upsert_kg_edge(&edge(&id, &change, Predicate::GeneratedChange))?;
            self.kg_clear_finding("MissingPlanId", &change)?;
            if let Some(plan) = plan
                .as_ref()
                .filter(|plan| self.get_kg_node(plan).ok().flatten().is_some())
            {
                self.upsert_kg_edge(&edge(&change, plan, Predicate::Implements))?;
            } else {
                self.kg_finding(
                    "MissingPlanId",
                    &change,
                    "The generating turn has no locally resolved governing intent",
                )?;
            }
            self.kg_clear_finding("MissingProvenance", &change)?;
            if has_provenance {
                self.upsert_kg_edge(&edge(&change, &provenance, Predicate::ChangeExplainedBy))?;
            } else {
                self.kg_finding(
                    "MissingProvenance",
                    &change,
                    "The generating turn's provenance sidecar is absent",
                )?;
            }
            if !self
                .get_kg_edges_from(&change)?
                .iter()
                .any(|edge| edge.kind == Predicate::AttestedBy)
            {
                self.kg_finding(
                    "MissingAttestation",
                    &change,
                    "No covering attestation is registered locally",
                )?;
            }
        }
        if let Some(previous) = previous {
            let previous = iri::mint_turn(&project, &turn.session_id, previous);
            if self.get_kg_node(&previous)?.is_some() {
                self.upsert_kg_edge(&edge(&id, &previous, Predicate::ProvWasInformedBy))?;
            }
        }
        Ok(())
    }

    pub fn project_session_kg(
        &mut self,
        record: &SessionRecord,
        turns: &[SessionTurn],
    ) -> PristineResult<()> {
        if !self.kg_writes_enabled()? {
            return Ok(());
        }
        self.emit_session_node(record)?;
        let numbers: std::collections::HashMap<Hash, u32> = turns
            .iter()
            .map(|turn| (turn.provenance_hash, turn.turn_number))
            .collect();
        for turn in turns {
            self.emit_turn_kg(
                record,
                turn,
                turn.previous_provenance
                    .and_then(|hash| numbers.get(&hash).copied()),
            )?;
        }
        let lineage = {
            use crate::change::session::SessionManifest;
            use crate::pristine::tables::{SESSION_HEADS, SESSION_MANIFESTS};
            let heads = self.txn.open_table(SESSION_HEADS)?;
            let manifests = self.txn.open_table(SESSION_MANIFESTS)?;
            let decode = |bytes: &[u8]| {
                SessionManifest::from_bytes(bytes).map_err(|error| PristineError::Serialization {
                    message: error.to_string(),
                })
            };
            let mut lineage = None;
            if let Some(head) = heads.get(record.session_id.as_str())? {
                if let Some(bytes) = manifests.get(head.value())? {
                    let manifest = decode(bytes.value())?;
                    if let (Some(parent), Some(turn)) =
                        (manifest.parent_session, manifest.fork_turn)
                    {
                        if let Some(bytes) = manifests.get(parent.as_bytes())? {
                            lineage = Some((decode(bytes.value())?.session_id, turn, parent));
                        }
                    }
                }
            }
            lineage
        };
        if let Some((parent, turn, manifest)) = lineage {
            let parent_id = iri::mint_session(&self.kg_project()?, &parent);
            if self.get_kg_node(&parent_id)?.is_none() {
                self.upsert_kg_node(&KgNode::new(
                    parent_id,
                    Class::Session,
                    &parent,
                    "session_ledger",
                ))?;
            }
            self.emit_session_fork_kg(&record.session_id, &parent, turn, &manifest)?;
        }
        Ok(())
    }

    pub fn emit_session_fork_kg(
        &mut self,
        child: &str,
        parent: &str,
        fork_turn: u32,
        parent_manifest: &Hash,
    ) -> PristineResult<()> {
        if !self.kg_writes_enabled()? {
            return Ok(());
        }
        let project = self.kg_project()?;
        let child_id = iri::mint_session(&project, child);
        let parent_id = iri::mint_session(&project, parent);
        if self.get_kg_node(&child_id)?.is_some() && self.get_kg_node(&parent_id)?.is_some() {
            self.upsert_kg_edge(&KgEdge::new(child_id, parent_id, Predicate::ProvWasDerivedFrom)
                .with_metadata(serde_json::json!({"fork_turn": fork_turn, "parent_manifest": parent_manifest.to_base32()})))?;
        }
        Ok(())
    }
}
