//! Read-only, change-specific plan declarations and intent-document associations.
//! Editing/creating an intent is NOT a declaration that it governs a turn.
use super::Repository;
use atomic_core::change::{Change, ProvenanceEdgeKind, ProvenanceGraph, ProvenanceNodeKind};
use atomic_core::pristine::vault::{VaultEntry, VaultEntryType};
use atomic_core::types::Hash;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Current vault identity, not a historical signed intent snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeIntentIdentity {
    pub id: String,
    pub uid: String,
    pub title: String,
    pub path: String,
}

/// An actual plan_id field recorded in a covering graph or its exact indexed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePlanBinding {
    pub plan_id: String,
    pub source: String,
    pub session_id: String,
    pub provenance_hash: Hash,
    pub turn_number: Option<u32>,
    pub intent: Option<ChangeIntentIdentity>,
}

/// A changed intent document. Its presence never supplies a missing plan_id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeIntentAssociation {
    pub path: String,
    pub intent: Option<ChangeIntentIdentity>,
    pub commitment_node_ids: Vec<String>,
    pub patch_node_ids: Vec<String>,
    pub provenance_hashes: Vec<Hash>,
}

/// Bounded observations for one change. No shape, signature, or uniqueness gate
/// runs here; `status` describes only the recorded plan references returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeIntentContext {
    pub plan_id: Option<String>,
    pub status: String, // recorded | missing | ambiguous | partial
    pub plans: Vec<ChangePlanBinding>,
    pub associated_intents: Vec<ChangeIntentAssociation>,
    pub warnings: Vec<String>,
}

impl Repository {
    /// Resolve recorded governing references for a selected change, retaining
    /// document-edit associations separately. Uses only read transactions.
    ///
    /// A session turn must match BOTH the provenance identity and selected
    /// change hash. Session creation links, tool commands, filenames and other
    /// turns' plans never become a governing plan declaration.
    pub fn change_intent_context(
        &self,
        hash: &Hash,
        change: &Change,
        graphs: &[(Hash, ProvenanceGraph)],
    ) -> ChangeIntentContext {
        let mut warnings = Vec::new();
        let mut paths: Vec<_> = crate::get_files_in_change(change);
        paths.extend(change.file_ops().iter().map(|op| op.path().to_string()));
        let mut associations = BTreeMap::new();
        for path in paths {
            let Some(path) = normalized_path(self.root(), &path) else {
                continue;
            };
            if !path.starts_with(".vault/intents/") || !path.ends_with(".md") {
                continue;
            }
            let entry = self
                .vault_retrieve(path.trim_start_matches(".vault/"))
                .ok()
                .flatten();
            if !path.ends_with("/intent.md")
                && !entry
                    .as_ref()
                    .is_some_and(|entry| entry.entry_type == VaultEntryType::Intent)
            {
                continue;
            }
            if associations.len() >= 128 && !associations.contains_key(&path) {
                warnings.push(
                    "Intent-document associations exceed the 128-entry display limit.".into(),
                );
                break;
            }
            associations
                .entry(path.clone())
                .or_insert_with(|| ChangeIntentAssociation {
                    intent: entry.and_then(|entry| identity(&entry, &path)),
                    path,
                    commitment_node_ids: vec![],
                    patch_node_ids: vec![],
                    provenance_hashes: vec![],
                });
        }
        let mut plans = Vec::new();
        let mut sessions = BTreeMap::new();
        if graphs.len() > 32 {
            warnings.push(
                "Plan resolution limited to 32 covering provenance graphs; coverage is partial."
                    .into(),
            );
        }
        for (graph_hash, graph) in graphs.iter().take(32) {
            if !graph.changes_explained.contains(hash)
                && !graph.nodes.iter().any(|node| {
                    node.kind == ProvenanceNodeKind::PatchProposal
                        && node.change_hash == Some(*hash)
                })
            {
                warnings.push(
                    "A returned graph does not identify the selected change; its plan was ignored."
                        .into(),
                );
                continue;
            }
            if let Some(plan) = &graph.plan_id {
                self.push_change_plan(
                    &mut plans,
                    &mut warnings,
                    plan,
                    graph,
                    graph_hash,
                    None,
                );
            }
            if !graph.session_id.is_empty() {
                if !sessions.contains_key(&graph.session_id) {
                    match self.get_session_ledger(&graph.session_id) {
                        Ok(ledger) => {
                            sessions
                                .insert(graph.session_id.clone(), ledger.map(|(_, turns)| turns));
                        }
                        Err(_) => {
                            warnings.push(format!(
                                "Session {} could not be read; turn-plan coverage is partial.",
                                graph.session_id
                            ));
                            sessions.insert(graph.session_id.clone(), None);
                        }
                    }
                }
                if let Some(Some(turns)) = sessions.get(&graph.session_id) {
                    for turn in turns.iter().filter(|turn| {
                        turn.session_id == graph.session_id
                            && turn.provenance_hash == *graph_hash
                            && turn.change_hashes.contains(hash)
                    }) {
                        if let Some(plan) = &turn.plan_id {
                            self.push_change_plan(
                                &mut plans,
                                &mut warnings,
                                plan,
                                graph,
                                graph_hash,
                                Some(turn.turn_number),
                            );
                        }
                    }
                }
            }
            if graph.nodes.len() > 2000 || graph.edges.len() > 4000 {
                warnings.push("Causal intent-edit evidence exceeds the graph display limit; coverage is partial.".into());
            }
            let patch_ids: BTreeSet<_> = graph
                .nodes
                .iter()
                .take(2000)
                .filter(|node| {
                    node.kind == ProvenanceNodeKind::PatchProposal
                        && node.change_hash == Some(*hash)
                })
                .map(|node| node.id.as_str())
                .collect();
            let committed: BTreeSet<_> = graph
                .edges
                .iter()
                .take(4000)
                .filter(|edge| {
                    edge.kind == ProvenanceEdgeKind::CommittedVia
                        && patch_ids.contains(edge.to.as_str())
                })
                .map(|edge| edge.from.as_str())
                .collect();
            for node in graph.nodes.iter().take(2000).filter(|node| {
                node.kind == ProvenanceNodeKind::Commitment && committed.contains(node.id.as_str())
            }) {
                let detail = node
                    .detail
                    .as_deref()
                    .filter(|detail| detail.len() <= 32768)
                    .and_then(|detail| serde_json::from_str::<serde_json::Value>(detail).ok());
                let Some(detail) = detail else {
                    continue;
                };
                for field in ["file_path", "path", "target", "file"] {
                    let Some(path) = detail
                        .get(field)
                        .and_then(|value| value.as_str())
                        .and_then(|path| normalized_path(self.root(), path))
                    else {
                        continue;
                    };
                    let Some(association) = associations.get_mut(&path) else {
                        continue;
                    };
                    association.commitment_node_ids.push(node.id.clone());
                    association.provenance_hashes.push(*graph_hash);
                    association.patch_node_ids.extend(
                        graph
                            .edges
                            .iter()
                            .take(4000)
                            .filter(|edge| {
                                edge.from == node.id
                                    && edge.kind == ProvenanceEdgeKind::CommittedVia
                                    && patch_ids.contains(edge.to.as_str())
                            })
                            .map(|edge| edge.to.clone()),
                    );
                    break;
                }
            }
        }
        for association in associations.values_mut() {
            association.commitment_node_ids.sort();
            association.commitment_node_ids.dedup();
            association.patch_node_ids.sort();
            association.patch_node_ids.dedup();
            if association.commitment_node_ids.len() > 256 || association.patch_node_ids.len() > 256
            {
                warnings.push(format!(
                    "{}: causal edit references truncated to 256 entries.",
                    association.path
                ));
                association.commitment_node_ids.truncate(256);
                association.patch_node_ids.truncate(256);
            }
            association.provenance_hashes.sort_by_key(|hash| hash.0);
            association.provenance_hashes.dedup();
            if association.intent.is_none() {
                warnings.push(format!(
                    "{}: current vault intent identity unavailable.",
                    association.path
                ));
            }
        }
        plans.sort_by(|a, b| (a.source != "provenance_graph").cmp(&(b.source != "provenance_graph"))
            .then(a.provenance_hash.0.cmp(&b.provenance_hash.0)).then(a.turn_number.cmp(&b.turn_number)).then(a.plan_id.cmp(&b.plan_id)));
        plans.dedup();
        let keys: BTreeSet<_> = plans
            .iter()
            .map(|binding: &ChangePlanBinding| {
                binding
                    .intent
                    .as_ref()
                    .map(|intent| {
                        if intent.uid.is_empty() {
                            intent.id.clone()
                        } else {
                            intent.uid.clone()
                        }
                    })
                    .unwrap_or_else(|| binding.plan_id.clone())
            })
            .collect();
        let plan_id = if keys.len() == 1 {
            plans.first().map(|binding| binding.plan_id.clone())
        } else {
            None
        };
        let status = if keys.len() > 1 {
            "ambiguous"
        } else if !warnings.is_empty() {
            "partial"
        } else if plans.is_empty() {
            "missing"
        } else {
            "recorded"
        };
        warnings.sort();
        warnings.dedup();
        warnings.truncate(100);
        ChangeIntentContext {
            plan_id,
            status: status.into(),
            plans,
            associated_intents: associations.into_values().collect(),
            warnings,
        }
    }

    fn push_change_plan(
        &self,
        plans: &mut Vec<ChangePlanBinding>,
        warnings: &mut Vec<String>,
        plan: &str,
        graph: &ProvenanceGraph,
        graph_hash: &Hash,
        turn: Option<u32>,
    ) {
        if plans.len() >= 128 || plan.len() > 512 || plan.trim().is_empty() {
            warnings.push("Plan declarations exceed the display limit or contain an invalid ID; coverage is partial.".into());
            return;
        }
        let intent = self
            .vault_intent_path(plan)
            .ok()
            .flatten()
            .and_then(|path| {
                let path = if path.starts_with(".vault/") {
                    path
                } else {
                    format!(".vault/{path}")
                };
                let path = normalized_path(self.root(), &path)?;
                let entry = self
                    .vault_retrieve(path.trim_start_matches(".vault/"))
                    .ok()
                    .flatten()?;
                identity(&entry, &path)
            });
        if intent.is_none() {
            warnings.push(format!(
                "Recorded plan {plan}: current vault intent identity could not be resolved."
            ));
        }
        plans.push(ChangePlanBinding {
            plan_id: plan.into(),
            source: if turn.is_some() { "session_turn" } else { "provenance_graph" }.into(),
            session_id: graph.session_id.clone(),
            provenance_hash: *graph_hash,
            turn_number: turn,
            intent,
        });
    }
}

fn normalized_path(root: &std::path::Path, input: &str) -> Option<String> {
    if input.len() > 32768 || input.contains('\0') {
        return None;
    }
    use std::path::{Component, Path};
    let path = Path::new(input);
    let path = if path.is_absolute() {
        path.strip_prefix(root).ok()?
    } else {
        path
    };
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn identity(entry: &VaultEntry, path: &str) -> Option<ChangeIntentIdentity> {
    if entry.entry_type != VaultEntryType::Intent || !path.starts_with(".vault/intents/") {
        return None;
    }
    let fm: serde_json::Value = serde_json::from_str(&entry.frontmatter_json).ok()?;
    let uid = fm
        .get("uid")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let id = fm
        .get("id")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
        .unwrap_or(&uid)
        .to_string();
    if id.is_empty() || id.len() > 512 {
        return None;
    }
    Some(ChangeIntentIdentity {
        id,
        uid,
        title: fm
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string(),
        path: path.into(),
    })
}
