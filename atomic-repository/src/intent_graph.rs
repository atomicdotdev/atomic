//! Intent-rooted, predicate-scoped selection over one semantic workflow snapshot.
//!
//! Shared sessions, files and identities are boundaries, not routes to unrelated
//! work. This is a read model, not OWL inference or source-code interpretation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use atomic_core::{
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
    },
    KgEdge, KgNode,
};
use serde::Serialize;

use crate::{repository::WorkflowSnapshot, Repository, RepositoryError};

pub const DEFAULT_MAX_NODES: usize = 5000;
pub const HARD_MAX_NODES: usize = 10_000;

#[derive(Clone, Debug, Serialize)]
pub struct IntentGraph {
    pub schema_version: u32,
    pub root: String,
    pub snapshot_identity: String,
    pub generation: u32,
    pub ontology_version: String,
    /// Completeness is relative to the asserted snapshot, not all historical work.
    pub scope: &'static str,
    pub nodes: Vec<KgNode>,
    pub edges: Vec<KgEdge>,
    pub implementing_changes: Vec<String>,
    /// Implementation backed by a captured turn path or its existing shortcut.
    pub captured_changes: Vec<String>,
    /// Explicit author declarations; never imply reconstructed turn provenance.
    pub declared_changes: Vec<String>,
    /// The selected intent's own vault artifact, joined from stored vault_path
    /// metadata to a canonical File node. No intent-to-file edge is synthesized.
    pub intent_artifact: Option<String>,
    /// Changes with an asserted modifiesFile edge to that exact artifact.
    /// Recording an intent is evidence, not an implementation assertion.
    pub recording_changes: Vec<String>,
    pub dependency_changes: Vec<String>,
    pub truncated: bool,
    pub missing_resources: Vec<String>,
    pub diagnostics: Vec<String>,
}

impl Repository {
    /// Resolve a human intent key, UID, or canonical intent IRI and select its
    /// asserted semantic lineage. Does not enrich/rebuild the graph or read files.
    pub fn intent_graph(&self, id: &str, max_nodes: usize) -> Result<IntentGraph, RepositoryError> {
        let uid = if let Ok((project, uid)) = iri::parse_intent(id) {
            if project != self.project_id()? {
                return Err(invalid("intent belongs to a different project"));
            }
            uid
        } else {
            let inputs = crate::intent_bridge::read_intent(self, id)?;
            inputs
                .frontmatter
                .get("uid")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid("intent has no canonical UID"))?
                .to_uppercase()
        };
        let root = iri::mint_intent(&self.project_id()?, &uid)
            .map_err(|error| invalid(&error.to_string()))?;
        IntentGraph::from_snapshot(&self.workflow_snapshot()?, &root, max_nodes)
    }
}

fn invalid(message: &str) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: message.into(),
    }
}

/// Traversal roles are deliberately separate from class assertions: a related
/// intent is a boundary unless it is the selected root.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Role {
    Root,
    IntentArtifact,
    Detail,
    Change,
    Turn,
    Attestation,
    View,
}

impl IntentGraph {
    /// Select from a consistent asserted snapshot. Limits stop traversal rather
    /// than silently treating a depth-limited neighborhood as complete lineage.
    pub fn from_snapshot(
        snapshot: &WorkflowSnapshot,
        root: &str,
        max_nodes: usize,
    ) -> Result<Self, RepositoryError> {
        if !(1..=HARD_MAX_NODES).contains(&max_nodes) {
            return Err(invalid("max-nodes must be between 1 and 10000"));
        }
        if snapshot.generation != atomic_core::kg::KG_GENERATION
            || snapshot.ontology_version != atomic_core::kg::ontology::VERSION_IRI
        {
            return Err(invalid(
                "unsupported workflow snapshot generation or ontology; rebuild the knowledge graph",
            ));
        }
        iri::parse_intent(root).map_err(|error| invalid(&error.to_string()))?;
        // These three resources are defined by the embedded ontology rather than
        // persisted as KG rows. Include them when an asserted edge references one.
        let statuses: Vec<_> = ["Verified", "Failed", "Unverifiable"]
            .into_iter()
            .map(|name| {
                KgNode::new(
                    format!("https://atomic.dev/ns/workflow#{name}"),
                    C::VerificationStatus,
                    name,
                    "ontology",
                )
            })
            .collect();
        let nodes: BTreeMap<&str, &KgNode> = snapshot
            .nodes
            .iter()
            .chain(statuses.iter())
            .map(|n| (n.id.as_str(), n))
            .collect();
        if !nodes.get(root).is_some_and(|node| {
            node.has_type(C::WorkIntent)
                || node
                    .types
                    .iter()
                    .filter_map(|t| C::from_iri(t))
                    .any(|c| c.parents().iter().any(|p| p.iri() == C::WorkIntent.iri()))
        }) {
            return Err(invalid("intent is not present in the workflow snapshot; run atomic query enrich to rebuild it"));
        }
        let mut outgoing: BTreeMap<&str, Vec<&KgEdge>> = BTreeMap::new();
        let mut incoming: BTreeMap<&str, Vec<&KgEdge>> = BTreeMap::new();
        for edge in &snapshot.edges {
            outgoing.entry(&edge.from_id).or_default().push(edge);
            incoming.entry(&edge.to_id).or_default().push(edge);
        }
        // Sorting makes capped selection independent of storage/serialization order.
        for edges in outgoing.values_mut().chain(incoming.values_mut()) {
            edges.sort_by(|a, b| {
                (&a.from_id, a.kind, &a.to_id).cmp(&(&b.from_id, b.kind, &b.to_id))
            });
        }
        let mut selected = BTreeSet::from([root.to_owned()]);
        let mut queue = VecDeque::from([(root.to_owned(), Role::Root)]);
        let mut visited = BTreeSet::new();
        let mut missing = BTreeSet::new();
        let mut implementing = BTreeSet::new();
        let mut captured = BTreeSet::new();
        let mut declared = BTreeSet::new();
        let mut recording = BTreeSet::new();
        let mut dependencies = BTreeSet::new();
        let mut truncated = false;
        // Join two facts already present in the snapshot: the vault entry's
        // repository path and a recorded File node. Do not use task file refs,
        // shared source files, or naming/timestamp similarity as attribution.
        let intent_artifact = crate::workflow_usage::intent_artifact_id(nodes[root]).filter(|id| {
            nodes
                .get(id.as_str())
                .is_some_and(|node| node.has_type(C::File))
        });
        if let Some(artifact) = &intent_artifact {
            if selected.len() < max_nodes {
                selected.insert(artifact.clone());
                queue.push_back((artifact.clone(), Role::IntentArtifact));
            } else {
                truncated = true;
            }
        }
        while let Some((id, role)) = queue.pop_front() {
            if !visited.insert((id.clone(), role)) {
                continue;
            }
            let out = outgoing.get(id.as_str()).into_iter().flatten().copied();
            let inc = incoming.get(id.as_str()).into_iter().flatten().copied();
            for (edge, forward) in out.map(|e| (e, true)).chain(inc.map(|e| (e, false))) {
                let next = if forward { &edge.to_id } else { &edge.from_id };
                let predicate = edge.kind;
                let transition: Option<Option<Role>> = match (role, forward, predicate) {
                    (Role::IntentArtifact, false, P::ModifiesFile)
                        if nodes
                            .get(next.as_str())
                            .is_some_and(|node| node.has_type(C::Change)) =>
                    {
                        Some(Some(Role::Change))
                    }
                    (Role::Root, false, P::Implements) => Some(Some(Role::Change)),
                    (Role::Root, true, P::DeclaresImplementation) => Some(Some(Role::Change)),
                    (Role::Root, false, P::HasPlan) | (Role::Root, true, P::GovernsTurn) => {
                        Some(Some(Role::Turn))
                    }
                    (
                        Role::Root,
                        true,
                        P::HasTask
                        | P::HasAcceptanceCriterion
                        | P::HasScopeIn
                        | P::HasScopeOut
                        | P::HasScopeItem
                        | P::HasConstraint,
                    ) => Some(Some(Role::Detail)),
                    (Role::Root, true, _) => Some(None),
                    (
                        Role::Root,
                        false,
                        P::Reviews
                        | P::ReviewedBy
                        | P::Remediates
                        | P::LinkedIntent
                        | P::LinkedGoal
                        | P::ProvWasDerivedFrom
                        | P::InformedBy
                        | P::DependsOnIntent
                        | P::Blocks
                        | P::BlockedBy
                        | P::References,
                    ) => Some(None),
                    (Role::Detail, true, P::Touches | P::Satisfies | P::References) => Some(None),
                    (Role::Change, true, P::DirectlyDependsOnChange) => Some(Some(Role::Change)),
                    (Role::Change, true, P::AttestedBy)
                    | (Role::Change, false, P::CoversChange) => Some(Some(Role::Attestation)),
                    (Role::Change, false, P::GeneratedChange)
                    | (Role::Change, true, P::GeneratedBy) => Some(Some(Role::Turn)),
                    (Role::Change, true, P::DirectlyOnView) => Some(Some(Role::View)),
                    (
                        Role::Change,
                        true,
                        P::ModifiesFile
                        | P::Modifies
                        | P::AuthoredBy
                        | P::ChangeExplainedBy
                        | P::ExplainedBy
                        | P::Implements,
                    ) => Some(None),
                    // Only root-governed turns can seed implementing changes.
                    (Role::Turn, true, P::GeneratedChange)
                        if outgoing.get(id.as_str()).is_some_and(|edges| {
                            edges
                                .iter()
                                .any(|e| e.kind == P::HasPlan && e.to_id == root)
                        }) || incoming.get(id.as_str()).is_some_and(|edges| {
                            edges
                                .iter()
                                .any(|e| e.kind == P::GovernsTurn && e.from_id == root)
                        }) =>
                    {
                        Some(Some(Role::Change))
                    }
                    (Role::Turn, false, P::Attests) => Some(Some(Role::Attestation)),
                    (
                        Role::Turn,
                        true,
                        P::HasPlan
                        | P::HasTodo
                        | P::TurnExplainedBy
                        | P::ExplainedBy
                        | P::TurnOfSession,
                    )
                    | (Role::Turn, false, P::HasTurn) => Some(None),
                    // Coverage is a boundary: never acquire sibling changes via an attestation.
                    (
                        Role::Attestation,
                        true,
                        P::SignedBy
                        | P::AttestsSession
                        | P::Attests
                        | P::PreviousAttestation
                        | P::VerificationResult,
                    ) => Some(None),
                    (Role::View, true, P::ChildViewOf) => Some(Some(Role::View)),
                    (_, false, P::AboutResource) => Some(None),
                    _ => None,
                };
                let Some(next_role) = transition else {
                    continue;
                };
                if !nodes.contains_key(next.as_str()) {
                    missing.insert(next.clone());
                    continue;
                }
                if !selected.contains(next) {
                    if selected.len() == max_nodes {
                        truncated = true;
                        continue;
                    }
                    selected.insert(next.clone());
                }
                if next_role == Some(Role::Change) {
                    if role == Role::IntentArtifact && predicate == P::ModifiesFile {
                        recording.insert(next.clone());
                    } else if predicate == P::Implements
                        || (role == Role::Turn && predicate == P::GeneratedChange)
                    {
                        implementing.insert(next.clone());
                        captured.insert(next.clone());
                    } else if predicate == P::DeclaresImplementation {
                        implementing.insert(next.clone());
                        declared.insert(next.clone());
                    } else if predicate == P::DirectlyDependsOnChange {
                        dependencies.insert(next.clone());
                    }
                }
                if let Some(next_role) = next_role {
                    queue.push_back((next.clone(), next_role));
                }
            }
        }
        // Include findings on boundary nodes too, without traversing from them.
        for id in selected.clone() {
            for edge in incoming
                .get(id.as_str())
                .into_iter()
                .flatten()
                .filter(|e| e.kind == P::AboutResource)
            {
                if !nodes.contains_key(edge.from_id.as_str()) {
                    missing.insert(edge.from_id.clone());
                } else if selected.len() < max_nodes || selected.contains(&edge.from_id) {
                    selected.insert(edge.from_id.clone());
                } else {
                    truncated = true;
                }
            }
        }
        dependencies.retain(|id| !implementing.contains(id) && !recording.contains(id));
        let selected_nodes: Vec<_> = selected
            .iter()
            .filter_map(|id| nodes.get(id.as_str()).map(|n| (*n).clone()))
            .collect();
        let mut edges: Vec<_> = snapshot
            .edges
            .iter()
            .filter(|e| selected.contains(&e.from_id) && selected.contains(&e.to_id))
            .cloned()
            .collect();
        edges.sort_by(|a, b| (&a.from_id, a.kind, &a.to_id).cmp(&(&b.from_id, b.kind, &b.to_id)));
        let mut diagnostics = vec!["Stored semantic graph facts, including exact intent-artifact recording evidence; no source interpretation, implicit ancestry, or historical patch/preimage expansion.".into()];
        if implementing.is_empty() {
            if recording.is_empty() {
                diagnostics.push("No implementing changes or intent-artifact recording changes are linked in this snapshot; this is not proof that no work occurred.".into());
            } else {
                diagnostics.push(format!("{} recorded change(s) modify this intent's artifact and are shown as recording evidence. No explicit implementation association is asserted; recording evidence does not establish governing-turn provenance.", recording.len()));
            }
        }
        if !declared.is_empty() {
            diagnostics.push(format!("{} implementing change(s) have explicit author declarations. Declarations do not repair or prove missing original plan_id/generating-turn provenance.", declared.len()));
        }
        if truncated {
            diagnostics.push("Node limit reached: selection and dependency closure are incomplete. Increase --max-nodes.".into());
        }
        if !missing.is_empty() {
            diagnostics.push("Some referenced resources are missing from the snapshot.".into());
        }
        for node in &selected_nodes {
            if node.has_type(C::Change) {
                let availability = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("object_availability"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let completeness = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("projection_completeness"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("reference-only");
                if availability != "available" || completeness != "projected" {
                    diagnostics.push(format!(
                        "{}: object {availability}, projection {completeness}",
                        node.id
                    ));
                }
            }
        }
        Ok(Self {
            schema_version: 1,
            root: root.into(),
            snapshot_identity: snapshot.identity.clone(),
            generation: snapshot.generation,
            ontology_version: snapshot.ontology_version.into(),
            scope: "intent_asserted_lineage_and_recording_evidence_with_dependency_closure",
            nodes: selected_nodes,
            edges,
            implementing_changes: implementing.into_iter().collect(),
            captured_changes: captured.into_iter().collect(),
            declared_changes: declared.into_iter().collect(),
            intent_artifact,
            recording_changes: recording.into_iter().collect(),
            dependency_changes: dependencies.into_iter().collect(),
            truncated,
            missing_resources: missing.into_iter().collect(),
            diagnostics,
        })
    }
}
