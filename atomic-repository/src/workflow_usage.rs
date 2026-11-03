//! Read-only usage summaries over immutable change provenance and KG membership.
//! Totals are scoped before graph selection/caps, never inferred from shared files.

use std::collections::{BTreeMap, BTreeSet};

use atomic_core::{
    change::Change,
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
    },
    types::{Base32, Hash},
    KgEdge, KgNode,
};
use serde::{Deserialize, Serialize};

use crate::{Repository, RepositoryError};

pub(crate) struct ViewUsageScope {
    changes: BTreeSet<String>,
    error: Option<String>,
}

/// Capture authoritative view filters in the same read transaction as KG rows.
/// KG membership edges can lag record/insert operations and are not accounting
/// authority for effective view membership.
pub(crate) fn view_scopes(
    repo: &Repository,
    txn: &impl atomic_core::pristine::ViewTxnT,
    project: &atomic_core::kg::project_id::ProjectId,
) -> Result<BTreeMap<String, ViewUsageScope>, RepositoryError> {
    let mut scopes = BTreeMap::new();
    let mut dependency_cache = BTreeMap::<Hash, Vec<Hash>>::new();
    for name in txn.list_views()? {
        let Some(view) = txn.get_view(&name)? else {
            continue;
        };
        let mut scope = ViewUsageScope {
            changes: BTreeSet::new(),
            error: None,
        };
        // Accounting does not traverse the byte graph. Resolve canonical view
        // membership, then verify dependency headers directly rather than
        // requiring old repositories to backfill a traversal acceleration index.
        match crate::repository::view_membership(txn, &view) {
            Ok(membership) => {
                let mut pending = Vec::new();
                for change in membership.iter() {
                    if let Some(hash) = txn.get_external(*change)? {
                        pending.push(hash);
                    } else {
                        scope.error =
                            Some("A canonical view member has no external change hash".into());
                    }
                }
                while let Some(hash) = pending.pop() {
                    if !scope.changes.insert(iri::mint_change(&hash)) {
                        continue;
                    }
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        dependency_cache.entry(hash)
                    {
                        match repo.load_change(&hash) {
                            Ok(change) => {
                                entry.insert(change.dependencies().to_vec());
                            }
                            Err(RepositoryError::ChangeNotFound { .. }) => {
                                scope.error = Some(format!("View change {} is unavailable; dependency closure may be incomplete", hash.to_base32()));
                                continue;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    for dependency in &dependency_cache[&hash] {
                        if txn.get_internal(dependency)?.is_none() {
                            scope.error = Some(format!(
                                "View dependency {} is not registered in the canonical graph",
                                dependency.to_base32()
                            ));
                        }
                        pending.push(*dependency);
                    }
                }
            }
            Err(error) => scope.error = Some(error.to_string()),
        }
        scopes.insert(iri::mint_view(project, &name), scope);
    }
    Ok(scopes)
}

impl Repository {
    /// Hydrate older KG projections from verified immutable change headers.
    /// This only changes the returned read model, never the repository database.
    pub(crate) fn hydrate_workflow_usage(
        &self,
        nodes: &mut [KgNode],
    ) -> Result<(), RepositoryError> {
        for node in nodes.iter_mut().filter(|node| node.has_type(C::Change)) {
            let metadata = node.metadata.as_ref();
            let availability = metadata
                .and_then(|m| m.get("object_availability"))
                .and_then(|v| v.as_str());
            if matches!(availability, Some("absent" | "unknown")) {
                set_change_records(node, None)?;
                continue;
            }
            if metadata
                .and_then(|m| m.get("ai_usage_record_version"))
                .and_then(|v| v.as_u64())
                == Some(1)
            {
                continue;
            }
            let hash = iri::parse_change(&node.id)
                .map_err(|error| RepositoryError::Serialization(error.to_string()))?;
            match self.load_change(&hash) {
                Ok(change) => set_change_records(node, Some(change_records(&change, &hash)))?,
                Err(RepositoryError::ChangeNotFound { .. }) => set_change_records(node, None)?,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Copy full-snapshot usage summaries into a selected subgraph. Aggregate
    /// scope does not shrink when the exported graph is capped or filtered.
    pub fn enrich_workflow_usage(&self, nodes: &mut [KgNode]) -> Result<(), RepositoryError> {
        if !nodes
            .iter()
            .any(|node| node.has_type(C::Change) || node.has_type(C::View) || is_intent(node))
        {
            return Ok(());
        }
        let snapshot = self.workflow_snapshot()?;
        let summaries: BTreeMap<_, _> = snapshot
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect();
        for node in nodes {
            if let Some(usage) = summaries
                .get(node.id.as_str())
                .and_then(|source| source.metadata.as_ref())
                .and_then(|metadata| metadata.get("ai_usage"))
            {
                metadata_object(node)?["ai_usage"] = usage.clone();
            }
        }
        Ok(())
    }
}

fn is_intent(node: &KgNode) -> bool {
    node.types
        .iter()
        .filter_map(|kind| C::from_iri(kind))
        .any(|kind| {
            kind == C::WorkIntent
                || kind
                    .parents()
                    .iter()
                    .any(|parent| parent.iri() == C::WorkIntent.iri())
        })
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Tokens {
    input: u64,
    output: u64,
    reasoning: u64,
    cache_read: u64,
    cache_write: u64,
    total: u64,
}

impl Tokens {
    fn add(&mut self, other: &Self) -> Result<(), RepositoryError> {
        self.input = add(self.input, other.input)?;
        self.output = add(self.output, other.output)?;
        self.reasoning = add(self.reasoning, other.reasoning)?;
        self.cache_read = add(self.cache_read, other.cache_read)?;
        self.cache_write = add(self.cache_write, other.cache_write)?;
        self.total = add(self.total, other.total)?;
        Ok(())
    }
}

fn invalid(message: &str) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: message.into(),
    }
}
fn add(a: u64, b: u64) -> Result<u64, RepositoryError> {
    a.checked_add(b)
        .ok_or_else(|| invalid("AI usage total exceeds u64; refusing to wrap or understate usage"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UsageRecord {
    id: String,
    vendor: String,
    model: String,
    tool: String,
    tokens: Option<Tokens>,
    cost_usd_micros: Option<u64>,
}

/// Only usage/attribution fields are exported: never prompts or reasoning text.
pub(crate) fn change_records(change: &Change, hash: &Hash) -> Vec<UsageRecord> {
    change
        .provenance()
        .iter()
        .enumerate()
        .map(|(index, provenance)| {
            let vendor = provenance.vendor.name().to_owned();
            let model = provenance.model.clone();
            let tool = provenance.tool.description();
            let turn = provenance
                .metadata
                .iter()
                .find(|(key, _)| key == "turn_number")
                .map(|(_, value)| value)
                .filter(|value| !value.is_empty());
            let identity = if let Some(request) = provenance
                .request_id
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                serde_json::to_vec(&("request", &vendor, request))
            } else if let (Some(session), Some(turn)) = (
                provenance
                    .session_id
                    .as_deref()
                    .filter(|value| !value.is_empty()),
                turn,
            ) {
                serde_json::to_vec(&("session_turn", session, turn, &vendor, &model, &tool))
            } else {
                // No identifiable shared generation: do not guess that otherwise
                // identical records on distinct changes are the same paid request.
                serde_json::to_vec(&("change_record", hash.to_base32(), index))
            }
            .expect("usage identity serialization");
            let tokens = &provenance.tokens;
            let cost = provenance.cost;
            let zero_reported = provenance
                .metadata
                .iter()
                .any(|(key, value)| key == "cost_reported" && value == "true");
            let cost_usd_micros = if cost.micro_usd > 0 || (cost.usd == 0.0 && zero_reported) {
                Some(cost.micro_usd)
            } else if cost.usd.is_finite()
                && cost.usd > 0.0
                && cost.usd * 1_000_000.0 < u64::MAX as f64
            {
                // Legacy records may predate the integer micro-dollar field.
                Some((cost.usd * 1_000_000.0).round() as u64)
            } else {
                None
            };
            UsageRecord {
                id: Hash::of(&identity).to_base32(),
                vendor,
                model,
                tool,
                tokens: if [
                    tokens.input_tokens,
                    tokens.output_tokens,
                    tokens.reasoning_tokens,
                    tokens.cache_read_tokens,
                    tokens.cache_write_tokens,
                    tokens.total_tokens,
                ]
                .iter()
                .all(|value| *value == 0)
                {
                    None
                } else {
                    Some(Tokens {
                        input: tokens.input_tokens,
                        output: tokens.output_tokens,
                        reasoning: tokens.reasoning_tokens,
                        cache_read: tokens.cache_read_tokens,
                        cache_write: tokens.cache_write_tokens,
                        total: tokens.total_tokens,
                    })
                },
                cost_usd_micros,
            }
        })
        .collect()
}

fn metadata_object(node: &mut KgNode) -> Result<&mut serde_json::Value, RepositoryError> {
    let metadata = node.metadata.get_or_insert_with(|| serde_json::json!({}));
    if !metadata.is_object() {
        return Err(invalid(
            "KG metadata must be an object for AI usage projection",
        ));
    }
    Ok(metadata)
}

pub(crate) fn set_change_records(
    node: &mut KgNode,
    records: Option<Vec<UsageRecord>>,
) -> Result<(), RepositoryError> {
    let metadata = metadata_object(node)?;
    metadata["ai_usage_record_version"] = 1.into();
    metadata["ai_usage_state"] = match &records {
        None => "unavailable",
        Some(records) if records.is_empty() => "not_recorded",
        Some(_) => "recorded",
    }
    .into();
    metadata["ai_usage_records"] = serde_json::to_value(records).expect("usage serialization");
    Ok(())
}

pub(crate) fn intent_artifact_id(node: &KgNode) -> Option<String> {
    let (project, _) = iri::parse_intent(&node.id).ok()?;
    let path = node.metadata.as_ref()?.get("vault_path")?.as_str()?;
    if path.is_empty()
        || path.contains('\\')
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return None;
    }
    Some(iri::mint_file(&project, &format!(".vault/{path}")))
}

#[derive(Default, Serialize)]
struct Coverage {
    changes: usize,
    with_provenance: usize,
    without_provenance: usize,
    unavailable: usize,
    unique_usage_records: usize,
    duplicate_usage_records: usize,
    conflicting_usage_records: usize,
    records_without_cost: usize,
    records_without_tokens: usize,
}

#[derive(Default, Serialize)]
struct Attribution {
    vendor: String,
    model: String,
    tool: String,
    records: usize,
    tokens: Option<Tokens>,
    cost_usd_micros: Option<u64>,
    cost_usd: Option<String>,
}

fn usd(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

fn summarize(
    selected: &BTreeSet<String>,
    nodes: &BTreeMap<&str, &KgNode>,
    basis: &str,
    scope_complete: bool,
) -> Result<serde_json::Value, RepositoryError> {
    let mut coverage = Coverage {
        changes: selected.len(),
        ..Default::default()
    };
    let mut records = BTreeMap::<String, UsageRecord>::new();
    let mut conflicts = BTreeSet::new();
    let mut attribution_keys = BTreeSet::new();
    for id in selected {
        let Some(node) = nodes
            .get(id.as_str())
            .filter(|node| node.has_type(C::Change))
        else {
            coverage.unavailable += 1;
            continue;
        };
        let metadata = node.metadata.as_ref();
        match metadata
            .and_then(|m| m.get("ai_usage_state"))
            .and_then(|value| value.as_str())
        {
            Some("not_recorded") => {
                coverage.without_provenance += 1;
                continue;
            }
            Some("recorded") => coverage.with_provenance += 1,
            _ => {
                coverage.unavailable += 1;
                continue;
            }
        }
        let entries: Vec<UsageRecord> = serde_json::from_value(
            metadata
                .and_then(|m| m.get("ai_usage_records"))
                .cloned()
                .unwrap_or_default(),
        )
        .map_err(|error| {
            RepositoryError::Serialization(format!("invalid AI usage projection: {error}"))
        })?;
        for record in entries {
            attribution_keys.insert((
                record.vendor.clone(),
                record.model.clone(),
                record.tool.clone(),
            ));
            if let Some(existing) = records.get(&record.id) {
                if existing == &record {
                    coverage.duplicate_usage_records += 1;
                } else {
                    conflicts.insert(record.id.clone());
                }
            } else {
                records.insert(record.id.clone(), record);
            }
        }
    }
    coverage.conflicting_usage_records = conflicts.len();
    records.retain(|id, _| !conflicts.contains(id));
    coverage.unique_usage_records = records.len();
    let mut totals = None::<Tokens>;
    let mut cost = None;
    let mut groups = BTreeMap::<(String, String, String), Attribution>::new();
    for record in records.values() {
        let group = groups
            .entry((
                record.vendor.clone(),
                record.model.clone(),
                record.tool.clone(),
            ))
            .or_insert_with(|| Attribution {
                vendor: record.vendor.clone(),
                model: record.model.clone(),
                tool: record.tool.clone(),
                ..Default::default()
            });
        group.records += 1;
        if let Some(tokens) = &record.tokens {
            totals.get_or_insert_with(Tokens::default).add(tokens)?;
            group
                .tokens
                .get_or_insert_with(Tokens::default)
                .add(tokens)?;
        } else {
            coverage.records_without_tokens += 1;
        }
        if let Some(amount) = record.cost_usd_micros {
            cost = Some(add(cost.unwrap_or(0), amount)?);
            group.cost_usd_micros = Some(add(group.cost_usd_micros.unwrap_or(0), amount)?);
        } else {
            coverage.records_without_cost += 1;
        }
    }
    // Attribution remains useful even when conflicting usage cannot be billed.
    for (vendor, model, tool) in attribution_keys {
        groups
            .entry((vendor.clone(), model.clone(), tool.clone()))
            .or_insert_with(|| Attribution {
                vendor,
                model,
                tool,
                ..Default::default()
            });
    }
    let contributors: Vec<_> = groups
        .into_values()
        .map(|mut group| {
            group.cost_usd = group.cost_usd_micros.map(usd);
            group
        })
        .collect();
    Ok(serde_json::json!({
        "schema_version": 1, "basis": basis, "scope_complete": scope_complete,
        "change_set_hash": Hash::of(&serde_json::to_vec(selected).expect("usage scope serialization")).to_base32(),
        "coverage": coverage,
        "tokens": totals,
        "cost": cost.map(|amount| serde_json::json!({"currency":"USD", "amount_micros":amount, "amount":usd(amount)})),
        "contributors": contributors,
        "notes": [
            "Recorded AI usage only, not total development cost. Missing provenance is not zero usage.",
            "Token totals are preserved as recorded. Cache and reasoning categories may overlap provider totals; do not add rows to reconstruct total.",
            "Zero token fields are stored values; older captures may default unreported fields to zero. All-zero token records and unmarked zero costs are treated as unreported, not free.",
            "Shared request/session-turn usage is counted once; conflicting copies are excluded rather than guessed."
        ]
    }))
}

/// Attach derived usage to change/intent/view nodes, using the complete snapshot.
#[cfg(test)]
pub(crate) fn annotate(nodes: &mut [KgNode], edges: &[KgEdge]) -> Result<(), RepositoryError> {
    annotate_with_views(nodes, edges, None)
}

pub(crate) fn annotate_with_views(
    nodes: &mut [KgNode],
    edges: &[KgEdge],
    canonical_views: Option<&BTreeMap<String, ViewUsageScope>>,
) -> Result<(), RepositoryError> {
    let by_id: BTreeMap<_, _> = nodes.iter().map(|node| (node.id.as_str(), node)).collect();
    let mut generated = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut intent_changes = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut recording = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut membership = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut parents = BTreeMap::<&str, BTreeSet<String>>::new();
    let mut dependencies = BTreeMap::<&str, BTreeSet<String>>::new();
    for edge in edges {
        let map_and_pair = match edge.kind {
            P::GeneratedChange => Some((&mut generated, (&edge.from_id, &edge.to_id))),
            P::Implements => Some((&mut intent_changes, (&edge.to_id, &edge.from_id))),
            P::DeclaresImplementation => Some((&mut intent_changes, (&edge.from_id, &edge.to_id))),
            P::ModifiesFile => Some((&mut recording, (&edge.to_id, &edge.from_id))),
            P::DirectlyOnView => Some((&mut membership, (&edge.to_id, &edge.from_id))),
            P::ChildViewOf => Some((&mut parents, (&edge.from_id, &edge.to_id))),
            P::DirectlyDependsOnChange => Some((&mut dependencies, (&edge.from_id, &edge.to_id))),
            _ => None,
        };
        if let Some((map, (from, to))) = map_and_pair {
            map.entry(from).or_default().insert(to.clone());
        }
    }
    for edge in edges {
        let (intent, turn) = match edge.kind {
            P::HasPlan => (&edge.to_id, &edge.from_id),
            P::GovernsTurn => (&edge.from_id, &edge.to_id),
            _ => continue,
        };
        if let Some(changes) = generated.get(turn.as_str()) {
            intent_changes
                .entry(intent)
                .or_default()
                .extend(changes.iter().cloned());
        }
    }
    let mut summaries = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let (mut selected, mut basis, mut scope_complete) = if node.has_type(C::Change) {
            (BTreeSet::from([node.id.clone()]), "change_provenance", true)
        } else if is_intent(node) {
            let mut selected = intent_changes
                .get(node.id.as_str())
                .cloned()
                .unwrap_or_default();
            if let Some(artifact) = intent_artifact_id(node)
                .filter(|id| by_id.get(id.as_str()).is_some_and(|n| n.has_type(C::File)))
            {
                if let Some(changes) = recording.get(artifact.as_str()) {
                    selected.extend(changes.iter().cloned());
                }
            }
            (
                selected,
                "intent_implementation_and_recording_evidence_excluding_dependencies",
                true,
            )
        } else if node.has_type(C::View) {
            (
                BTreeSet::new(),
                "view_effective_membership_including_ancestors_and_dependencies",
                true,
            )
        } else {
            continue;
        };
        let mut scope_error = None;
        if node.has_type(C::View) && canonical_views.is_some() {
            basis = "view_canonical_effective_membership_including_ancestors_and_dependencies";
            match canonical_views.and_then(|views| views.get(&node.id)) {
                Some(scope) => {
                    selected = scope.changes.clone();
                    scope_complete = scope.error.is_none();
                    scope_error = scope.error.clone();
                }
                None => {
                    scope_complete = false;
                    scope_error =
                        Some("View is not present in canonical repository metadata".into());
                }
            }
        } else if node.has_type(C::View) {
            let mut pending = vec![node.id.clone()];
            let mut visited = BTreeSet::new();
            while let Some(view) = pending.pop() {
                if !visited.insert(view.clone()) {
                    continue;
                }
                scope_complete &= by_id
                    .get(view.as_str())
                    .is_some_and(|node| node.has_type(C::View));
                if let Some(changes) = membership.get(view.as_str()) {
                    selected.extend(changes.iter().cloned());
                }
                if let Some(ancestors) = parents.get(view.as_str()) {
                    pending.extend(ancestors.iter().cloned());
                }
            }
            let mut pending: Vec<_> = selected.iter().cloned().collect();
            while let Some(change) = pending.pop() {
                if let Some(deps) = dependencies.get(change.as_str()) {
                    for dep in deps {
                        if selected.insert(dep.clone()) {
                            pending.push(dep.clone());
                        }
                    }
                }
            }
        }
        let mut summary = summarize(&selected, &by_id, basis, scope_complete)?;
        if let Some(error) = scope_error {
            summary["scope_error"] = error.into();
        }
        summaries.push((index, summary));
    }
    for (index, summary) in summaries {
        metadata_object(&mut nodes[index])?["ai_usage"] = summary;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{AITool, AIVendor, ChangeHeader, Provenance, TokenUsage};

    fn change(name: &str, amount: u64, request: &str) -> KgNode {
        let mut change = Change::empty(ChangeHeader::new(name));
        let mut provenance = Provenance::builder()
            .vendor(AIVendor::OpenAI)
            .model("model-a")
            .tool(AITool::cli("test-agent"))
            .request_id(request)
            .prompt_full("PRIVATE PROMPT")
            .reasoning_text("PRIVATE REASONING")
            .build();
        provenance.tokens = TokenUsage::full(100, 40, 10, 60, 20);
        provenance.tokens.total_tokens = 140; // Reasoning already included by this provider.
        provenance.cost = atomic_core::change::Cost::from_micro_usd(amount);
        change.add_provenance(provenance);
        let hash = Hash::of(name.as_bytes());
        let mut node = KgNode::new(iri::mint_change(&hash), C::Change, name, "vcs");
        set_change_records(&mut node, Some(change_records(&change, &hash))).unwrap();
        node
    }
    fn usage<'a>(nodes: &'a [KgNode], id: &str) -> &'a serde_json::Value {
        &nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()["ai_usage"]
    }

    #[test]
    fn intent_scopes_exclude_dependency_only_history_and_deduplicate_all_associations() {
        let first = change("first", 86_176, "request-a");
        let dependency = change("dependency", 90_964, "request-b");
        let project = atomic_core::kg::project_id::ProjectId::from_base32(&"A".repeat(52)).unwrap();
        let uid = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let intent = KgNode::new(
            iri::mint_intent(&project, uid).unwrap(),
            C::FeatureIntent,
            "intent",
            "vault",
        )
        .with_metadata(serde_json::json!({"vault_path":format!("intents/{uid}/intent.md")}));
        let artifact = KgNode::new(
            intent_artifact_id(&intent).unwrap(),
            C::File,
            "intent.md",
            "vcs",
        );
        let edges = vec![
            KgEdge::new(&first.id, &intent.id, P::Implements),
            KgEdge::new(&intent.id, &first.id, P::DeclaresImplementation),
            KgEdge::new(&first.id, &artifact.id, P::ModifiesFile),
            KgEdge::new("turn", &intent.id, P::HasPlan),
            KgEdge::new("turn", &first.id, P::GeneratedChange),
            KgEdge::new(&first.id, &dependency.id, P::DirectlyDependsOnChange),
        ];
        let mut nodes = vec![first.clone(), dependency, intent.clone(), artifact];
        annotate(&mut nodes, &edges).unwrap();
        let summary = usage(&nodes, &intent.id);
        assert_eq!(summary["coverage"]["changes"], 1);
        assert_eq!(summary["cost"]["amount_micros"], 86_176);
        assert_eq!(summary["cost"]["amount"], "0.086176");
        assert_eq!(summary["tokens"]["total"], 140);
        assert_eq!(summary["tokens"]["reasoning"], 10);
        assert_eq!(summary["tokens"]["cache_read"], 60);
        assert_eq!(summary["tokens"]["cache_write"], 20);
        assert_eq!(summary["contributors"][0]["model"], "model-a");
        assert_eq!(summary["contributors"][0]["vendor"], "openai");
        assert_eq!(summary["contributors"][0]["tool"], "CLI: test-agent");
        let serialized = serde_json::to_string(&nodes).unwrap();
        assert!(!serialized.contains("PRIVATE"));
    }

    #[test]
    fn view_costs_include_ancestor_membership_and_dependencies_once() {
        let first = change("first", 86_176, "a");
        let second = change("second", 90_964, "b");
        let child = KgNode::new("child", C::View, "child", "vcs");
        let parent = KgNode::new("parent", C::View, "parent", "vcs");
        let edges = vec![
            KgEdge::new("child", "parent", P::ChildViewOf),
            KgEdge::new("parent", "child", P::ChildViewOf), // cycles must terminate
            KgEdge::new(&first.id, "child", P::DirectlyOnView),
            KgEdge::new(&first.id, "parent", P::DirectlyOnView),
            KgEdge::new(&first.id, &second.id, P::DirectlyDependsOnChange),
        ];
        let mut nodes = vec![first, second, child, parent];
        annotate(&mut nodes, &edges).unwrap();
        let summary = usage(&nodes, "child");
        assert_eq!(summary["coverage"]["changes"], 2);
        assert_eq!(summary["cost"]["amount_micros"], 177_140);
        assert_eq!(summary["tokens"]["total"], 280);
    }

    #[test]
    fn shared_requests_are_not_billed_twice_and_conflicts_are_not_guessed() {
        let first = change("first", 123, "shared-request");
        let second = change("second", 123, "shared-request");
        let view = KgNode::new("view", C::View, "view", "vcs");
        let edges = vec![
            KgEdge::new(&first.id, "view", P::DirectlyOnView),
            KgEdge::new(&second.id, "view", P::DirectlyOnView),
        ];
        let mut nodes = vec![first.clone(), second, view.clone()];
        annotate(&mut nodes, &edges).unwrap();
        assert_eq!(usage(&nodes, "view")["cost"]["amount_micros"], 123);
        assert_eq!(
            usage(&nodes, "view")["coverage"]["duplicate_usage_records"],
            1
        );
        nodes[1] = change("second", 456, "shared-request");
        annotate(&mut nodes, &edges).unwrap();
        assert!(usage(&nodes, "view")["cost"].is_null());
        assert!(usage(&nodes, "view")["tokens"].is_null());
        assert_eq!(
            usage(&nodes, "view")["coverage"]["conflicting_usage_records"],
            1
        );
        assert_eq!(usage(&nodes, "view")["contributors"][0]["model"], "model-a");
    }

    #[test]
    fn missing_provenance_is_unknown_not_free_and_dangling_members_are_counted() {
        let mut human = KgNode::new("human", C::Change, "human", "vcs");
        set_change_records(&mut human, Some(vec![])).unwrap();
        let mut absent = KgNode::new("absent", C::Change, "absent", "vcs");
        set_change_records(&mut absent, None).unwrap();
        let view = KgNode::new("view", C::View, "view", "vcs");
        let edges = vec![
            KgEdge::new("human", "view", P::DirectlyOnView),
            KgEdge::new("absent", "view", P::DirectlyOnView),
            KgEdge::new("missing", "view", P::DirectlyOnView),
            KgEdge::new("view", "missing-parent", P::ChildViewOf),
        ];
        let mut nodes = vec![human, absent, view];
        annotate(&mut nodes, &edges).unwrap();
        let summary = usage(&nodes, "view");
        assert!(summary["cost"].is_null() && summary["tokens"].is_null());
        assert_eq!(summary["coverage"]["without_provenance"], 1);
        assert_eq!(summary["coverage"]["unavailable"], 2);
        assert_eq!(summary["scope_complete"], false);
    }

    #[test]
    fn checked_totals_reject_overflow_and_invalid_metadata() {
        let mut tokens = Tokens {
            input: u64::MAX,
            ..Default::default()
        };
        assert!(tokens
            .add(&Tokens {
                input: 1,
                ..Default::default()
            })
            .is_err());
        assert!(add(u64::MAX, 1).is_err());
        let mut node = KgNode::new("malformed", C::Change, "malformed", "vcs")
            .with_metadata(serde_json::json!("not an object"));
        assert!(set_change_records(&mut node, None).is_err());
    }

    #[test]
    fn attribution_preserves_multiple_models_and_unreported_usage_is_not_zero() {
        let mut first = change("first", 123, "a");
        let second = change("second", 456, "b");
        let mut records: Vec<UsageRecord> =
            serde_json::from_value(first.metadata.as_ref().unwrap()["ai_usage_records"].clone())
                .unwrap();
        let mut additional: Vec<UsageRecord> =
            serde_json::from_value(second.metadata.as_ref().unwrap()["ai_usage_records"].clone())
                .unwrap();
        additional[0].model = "model-b".into();
        additional[0].vendor = "anthropic".into();
        additional[0].tool = "Editor: example".into();
        records.extend(additional);
        set_change_records(&mut first, Some(records)).unwrap();
        let id = first.id.clone();
        let mut nodes = vec![first];
        annotate(&mut nodes, &[]).unwrap();
        assert_eq!(
            usage(&nodes, &id)["contributors"].as_array().unwrap().len(),
            2
        );
        assert_eq!(usage(&nodes, &id)["cost"]["amount_micros"], 579);

        let mut change = Change::empty(ChangeHeader::new("model known, usage unreported"));
        change.add_provenance(
            Provenance::builder()
                .vendor(AIVendor::OpenAI)
                .model("model-a")
                .build(),
        );
        let hash = Hash::of(b"unreported");
        let mut node = KgNode::new(iri::mint_change(&hash), C::Change, "unreported", "vcs");
        set_change_records(&mut node, Some(change_records(&change, &hash))).unwrap();
        let id = node.id.clone();
        let mut nodes = vec![node];
        annotate(&mut nodes, &[]).unwrap();
        assert!(usage(&nodes, &id)["tokens"].is_null());
        assert!(usage(&nodes, &id)["cost"].is_null());
        assert_eq!(usage(&nodes, &id)["contributors"][0]["model"], "model-a");
        assert_eq!(usage(&nodes, &id)["coverage"]["records_without_cost"], 1);
        assert_eq!(usage(&nodes, &id)["coverage"]["records_without_tokens"], 1);
    }

    #[test]
    fn session_turn_identity_deduplicates_split_changes_but_unidentified_records_remain_distinct() {
        let mut change = Change::empty(ChangeHeader::new("split turn"));
        change.add_provenance(
            Provenance::builder()
                .vendor(AIVendor::OpenAI)
                .model("model-a")
                .tokens(10, 20)
                .session_id("session")
                .metadata("turn_number", "1")
                .cost_usd(0.01)
                .build(),
        );
        let first = change_records(&change, &Hash::of(b"first"));
        let second = change_records(&change, &Hash::of(b"second"));
        assert_eq!(first[0].id, second[0].id);
        change.hashed.provenance[0].session_id = None;
        let first = change_records(&change, &Hash::of(b"first"));
        let second = change_records(&change, &Hash::of(b"second"));
        assert_ne!(first[0].id, second[0].id);
        change.hashed.provenance[0].cost = atomic_core::change::Cost::zero();
        assert!(change_records(&change, &Hash::of(b"zero"))[0]
            .cost_usd_micros
            .is_none());
        change.hashed.provenance[0]
            .metadata
            .push(("cost_reported".into(), "true".into()));
        assert_eq!(
            change_records(&change, &Hash::of(b"zero"))[0].cost_usd_micros,
            Some(0)
        );
    }

    #[test]
    fn canonical_view_scopes_override_stale_edges_and_identify_the_actual_change_set() {
        let first = change("first", 123, "a");
        let second = change("second", 123, "b");
        let view = KgNode::new("view", C::View, "view", "vcs");
        let stale_edges = vec![KgEdge::new(&first.id, "view", P::DirectlyOnView)];
        let mut scopes = BTreeMap::from([(
            "view".into(),
            ViewUsageScope {
                changes: BTreeSet::from([second.id.clone()]),
                error: None,
            },
        )]);
        let mut nodes = vec![first.clone(), second, view];
        annotate_with_views(&mut nodes, &stale_edges, Some(&scopes)).unwrap();
        let second_hash = usage(&nodes, "view")["change_set_hash"].clone();
        assert_eq!(usage(&nodes, "view")["cost"]["amount_micros"], 123);
        scopes.get_mut("view").unwrap().changes = BTreeSet::from([first.id]);
        annotate_with_views(&mut nodes, &stale_edges, Some(&scopes)).unwrap();
        assert_ne!(usage(&nodes, "view")["change_set_hash"], second_hash);
        scopes.get_mut("view").unwrap().changes.clear();
        scopes.get_mut("view").unwrap().error = Some("unavailable canonical membership".into());
        annotate_with_views(&mut nodes, &stale_edges, Some(&scopes)).unwrap();
        assert_eq!(usage(&nodes, "view")["scope_complete"], false);
        assert!(usage(&nodes, "view")["cost"].is_null());
    }
}
