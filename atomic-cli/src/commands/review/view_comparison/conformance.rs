//! Recorded governing declarations and criterion evidence, not inferred approval.
use std::collections::{BTreeMap, BTreeSet};

use atomic_canonical::{
    review_finding::CriterionOutcome, review_graph::CriterionReview, CanonicalNode,
};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{intent_bridge as bridge, Repository};
use serde::Serialize;
use serde_json::{json, Value};

use super::ViewComparison;
use crate::error::{CliError, CliResult};

#[derive(Debug, Default, Serialize)]
pub struct Conformance {
    pub scope: Option<IntentScope>,
    pub intents: Vec<ScopedIntent>,
    pub changes: Vec<GovernanceChange>,
    pub criteria: Vec<ComparedCriterion>,
    pub diagnostics: Vec<String>,
    #[serde(skip)]
    current_sources: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct IntentScope {
    pub view: String,
    pub current_inventory_hash: String,
}

#[derive(Debug, Serialize)]
pub struct ScopedIntent {
    pub id: String,
    pub human_key: String,
    pub title: String,
    pub status: String,
    pub view: String,
    pub source_kind: String,
    pub path: String,
    pub source_hash: String,
    pub definition_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GovernanceChange {
    pub hash: String,
    /// added_to_after / removed_from_after; neither implies patch replay.
    pub direction: String,
    pub paths: Vec<String>,
    pub intent_context: Value,
}

#[derive(Debug, Serialize)]
pub struct ComparedCriterion {
    pub id: String,
    pub intent: String,
    pub governing_changes: Vec<String>,
    pub before: Option<Declaration>,
    pub after: Option<Declaration>,
    pub current_context: Option<Declaration>,
}

#[derive(Debug, Serialize)]
pub struct Declaration {
    pub text: String,
    pub status: String,
    pub source_kind: String,
    pub view: Option<String>,
    pub path: String,
    pub source_hash: String,
    pub verified_by: Option<String>,
    pub evidence: Option<String>,
    pub verifications: Vec<atomic_canonical::node::VerificationRecord>,
    pub required_kinds: Vec<String>,
    pub tasks: Vec<TaskEvidence>,
}

#[derive(Debug, Serialize)]
pub struct TaskEvidence {
    pub id: String,
    pub text: String,
    pub status: String,
    /// Navigation hints only; touching a task file does not prove conformance.
    pub files: Vec<Value>,
}

#[derive(Clone)]
struct Definition {
    node: CanonicalNode,
    kind: &'static str,
    view: Option<String>,
    path: String,
    hash: String,
}

struct CurrentIntent {
    row: ScopedIntent,
    definition: Option<Definition>,
}

struct CurrentScope {
    intents: Vec<CurrentIntent>,
    hash: String,
    diagnostics: Vec<String>,
}

fn vault_path(path: &str) -> String {
    if path.starts_with(".vault/") {
        path.into()
    } else {
        format!(".vault/{path}")
    }
}

fn current_scope(repo: &Repository, view: &str) -> CliResult<CurrentScope> {
    let manifest = if repo.has_vault().map_err(CliError::Repository)? {
        repo.vault_manifest().map_err(CliError::Repository)?
    } else {
        Default::default()
    };
    let mut intents = Vec::new();
    let mut inventory = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for (key, summary) in manifest.intents {
        let inputs = if summary.vault_path.is_empty() {
            bridge::read_intent(repo, &key).map_err(CliError::Repository)?
        } else {
            let Some(entry) = repo
                .vault_retrieve(summary.vault_path.trim_start_matches(".vault/"))
                .map_err(CliError::Repository)?
            else {
                inventory.insert(key.clone(), "missing_definition".into());
                diagnostics.push(format!(
                    "{key}: vault definition missing; view assignment unavailable"
                ));
                continue;
            };
            bridge::inputs_from_entry(&entry).map_err(CliError::Repository)?
        };
        if inputs.frontmatter.get("view").and_then(Value::as_str) != Some(view) {
            continue;
        }
        let hash = bridge::source_content_hash(&inputs);
        inventory.insert(key.clone(), hash.clone());
        let path = vault_path(&summary.vault_path);
        let lifted = bridge::lift(&inputs);
        let (node, error) = match lifted {
            Ok(node) => (Some(node), None),
            Err(error) => {
                let error = error.to_string();
                diagnostics.push(format!(
                    "{key}: view-scoped intent definition unavailable: {error}"
                ));
                (None, Some(error))
            }
        };
        let row = ScopedIntent {
            id: node
                .as_ref()
                .map(|node| node.id.clone())
                .unwrap_or_else(|| key.clone()),
            human_key: node
                .as_ref()
                .map(|node| node.human_key.clone())
                .unwrap_or_else(|| key.clone()),
            title: inputs
                .frontmatter
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&summary.title)
                .into(),
            status: inputs
                .frontmatter
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or(&summary.status)
                .into(),
            view: view.into(),
            source_kind: "current_vault_context".into(),
            path: path.clone(),
            source_hash: hash.clone(),
            definition_error: error,
        };
        let definition = node.map(|node| Definition {
            node,
            kind: "current_vault_context",
            view: None,
            path,
            hash,
        });
        intents.push(CurrentIntent { row, definition });
    }
    let hash = Hash::of(
        &serde_json::to_vec(&inventory).map_err(|error| CliError::Internal(error.into()))?,
    )
    .to_base32();
    Ok(CurrentScope {
        intents,
        hash,
        diagnostics,
    })
}

struct Sources<'a> {
    before: Option<&'a str>,
    after: &'a str,
    paths: BTreeSet<String>,
    files: BTreeMap<String, Vec<Value>>,
}

fn recorded(bytes: &[u8], view: &str, path: &str) -> Result<Definition, String> {
    if bytes.len() > 1024 * 1024 {
        return Err("Recorded intent exceeds the 1 MiB definition budget".into());
    }
    let hash = Hash::of(bytes).to_base32();
    let text = std::str::from_utf8(bytes)
        .map_err(|error| error.to_string())?
        .replace("\r\n", "\n");
    let (front, body) = text
        .strip_prefix("---\n")
        .and_then(|text| text.split_once("\n---\n"))
        .ok_or("Recorded intent has no delimited YAML frontmatter")?;
    let inputs = bridge::LiftInputs {
        frontmatter: serde_yaml_ng::from_str(front).map_err(|error| error.to_string())?,
        body: body.into(),
    };
    let node = bridge::lift(&inputs).map_err(|error| error.to_string())?;
    Ok(Definition {
        node,
        kind: "recorded_view",
        view: Some(view.into()),
        path: path.into(),
        hash,
    })
}

fn matches(node: &CanonicalNode, reference: &str) -> bool {
    node.id == reference
        || node.human_key == reference
        || node.id.rsplit(':').next() == Some(reference)
}

fn declaration(
    definition: &Definition,
    id: &str,
    files: &BTreeMap<String, Vec<Value>>,
) -> Option<Declaration> {
    let criterion = definition
        .node
        .has_acceptance_criterion
        .iter()
        .find(|criterion| criterion.id == id)?;
    let tasks = definition.node.has_task.iter().filter(|task|task.satisfies.as_slice().contains(&id.to_string())).map(|task|TaskEvidence {
        id:task.id.clone(),text:task.text.clone(),status:task.task_status.clone(),
        files:task.touches_file.iter().map(|path| {
            let references = files.get(path).cloned().unwrap_or_default();
            json!({"path":path,"comparisons":references,"coverage":if references.is_empty(){"not_present_in_recorded_comparison"}else{"recorded_file_reference_only"}})
        }).collect(),
    }).collect();
    Some(Declaration {
        text: criterion.text.clone(),
        status: criterion.ac_status.clone(),
        source_kind: definition.kind.into(),
        view: definition.view.clone(),
        path: definition.path.clone(),
        source_hash: definition.hash.clone(),
        verified_by: criterion.verified_by.clone(),
        evidence: criterion.evidence.clone(),
        verifications: criterion.verifications.clone(),
        required_kinds: criterion.required_kinds.clone(),
        tasks,
    })
}

pub(super) fn collect(
    repo: &Repository,
    comparison: &ViewComparison,
    before: &[String],
    after: &[String],
) -> CliResult<Conformance> {
    let mut files = BTreeMap::<String, Vec<Value>>::new();
    for file in &comparison.files {
        for path in file
            .before_path
            .iter()
            .chain(&file.after_path)
            .collect::<BTreeSet<_>>()
        {
            files.entry(path.clone()).or_default().push(json!({"source_ref":file.source_ref,"before_path":file.before_path,"after_path":file.after_path,"status":file.status}));
        }
    }
    let sources = Sources {
        before: Some(&comparison.from.view),
        after: &comparison.to.view,
        paths: files.keys().cloned().collect(),
        files,
    };
    collect_sources(repo, &sources, before, after)
}

pub(in super::super) fn collect_view(
    repo: &Repository,
    snapshot: &super::super::incremental::ViewInspection,
) -> CliResult<Conformance> {
    let files=snapshot.paths.iter().map(|path|(path.clone(),vec![json!({"source_ref":snapshot.source_ref,"before_path":Value::Null,"after_path":path,"status":"snapshot"})])).collect();
    let sources = Sources {
        before: None,
        after: &snapshot.view,
        paths: snapshot.paths.iter().cloned().collect(),
        files,
    };
    collect_sources(repo, &sources, &[], &[])
}

fn collect_sources(
    repo: &Repository,
    sources: &Sources<'_>,
    before: &[String],
    after: &[String],
) -> CliResult<Conformance> {
    let old: BTreeSet<_> = before.iter().collect();
    let new: BTreeSet<_> = after.iter().collect();
    let mut result = Conformance::default();
    let current = current_scope(repo, sources.after)?;
    result.scope = Some(IntentScope {
        view: sources.after.into(),
        current_inventory_hash: current.hash,
    });
    result.diagnostics.extend(current.diagnostics);
    let mut current_definitions = Vec::new();
    let mut plans = BTreeMap::<String, BTreeSet<String>>::new();
    let mut candidates = BTreeSet::new();
    for intent in current.intents {
        candidates.insert(intent.row.path.clone());
        if let Some(definition) = intent.definition {
            plans.entry(definition.node.id.clone()).or_default();
            current_definitions.push(definition);
        }
        result.intents.push(intent.row);
    }
    for (hash, direction) in new
        .difference(&old)
        .map(|hash| (*hash, "added_to_after"))
        .chain(
            old.difference(&new)
                .map(|hash| (*hash, "removed_from_after")),
        )
    {
        let parsed = Hash::from_base32(hash.as_bytes()).expect("recorded membership hash");
        let change = repo.load_change(&parsed).map_err(CliError::Repository)?;
        let graphs = repo
            .find_provenance_for_change(&parsed)
            .map_err(CliError::Repository)?;
        let context = repo.change_intent_context(&parsed, &change, &graphs);
        if context.status == "recorded" {
            for plan in &context.plans {
                let reference = plan
                    .intent
                    .as_ref()
                    .map(|intent| {
                        if intent.uid.is_empty() {
                            intent.id.as_str()
                        } else {
                            intent.uid.as_str()
                        }
                    })
                    .unwrap_or(&plan.plan_id);
                plans
                    .entry(reference.into())
                    .or_default()
                    .insert(hash.clone());
                if let Some(intent) = &plan.intent {
                    candidates.insert(intent.path.clone());
                }
                if let Ok(Some(path)) = bridge::vault_path_for(repo, reference) {
                    candidates.insert(if path.starts_with(".vault/") {
                        path
                    } else {
                        format!(".vault/{path}")
                    });
                }
            }
        } else {
            result.diagnostics.push(format!(
                "{hash} ({direction}): {} governing intent; file overlap is not a declaration",
                context.status
            ));
        }
        let mut paths = repo
            .change_modified_paths(&parsed)
            .map_err(CliError::Repository)?;
        paths.sort();
        paths.dedup();
        result.changes.push(GovernanceChange {
            hash: hash.clone(),
            direction: direction.into(),
            paths,
            intent_context: serde_json::to_value(context)
                .map_err(|error| CliError::Internal(error.into()))?,
        });
    }
    result.changes.sort_by(|a, b| a.hash.cmp(&b.hash));
    // Recorded definitions can survive deletion of their current vault entry.
    // Inspect only intent documents, never unrelated history or code payloads.
    for path in &sources.paths {
        if path.starts_with(".vault/intents/") && path.ends_with("/intent.md") {
            candidates.insert(path.clone());
        }
    }
    let mut before_definitions = Vec::new();
    let mut after_definitions = Vec::new();
    if !candidates.is_empty() {
        let candidates: Vec<_> = candidates.into_iter().collect();
        for (view, definitions) in [
            (sources.before, &mut before_definitions),
            (Some(sources.after), &mut after_definitions),
        ] {
            let Some(view) = view else {
                continue;
            };
            // One canonical tree projection and span context per side, not one
            // full repository projection for every intent document.
            let mut remaining = 16 * 1024 * 1024usize;
            repo.for_each_file_content_and_kind_on_view(&candidates,view,|path,bytes,kind| {
                let Some(bytes)=bytes else {return Ok(());};
                if kind!=Some(atomic_core::change::InodeKind::Regular) || bytes.len()>remaining {
                    result.diagnostics.push(format!("{view}:{path}: recorded definition unavailable (non-regular source or 16 MiB intent-source budget); no acceptance claim inferred"));
                    return Ok(());
                }
                remaining -= bytes.len();
                match recorded(&bytes, view, path) {
                    Ok(definition)
                        if definition.node.view.as_deref()==Some(sources.after) || plans
                            .keys()
                            .any(|reference| matches(&definition.node, reference)) =>
                    {
                        definitions.push(definition)
                    }
                    Ok(_) => {}
                    Err(error) => result
                        .diagnostics
                        .push(format!("{view}:{path}: {error}")),
                }
                Ok(())
            }).map_err(CliError::Repository)?;
        }
    }
    for definition in &after_definitions {
        if definition.node.view.as_deref() != Some(sources.after) {
            continue;
        }
        plans.entry(definition.node.id.clone()).or_default();
        let row = ScopedIntent {
            id: definition.node.id.clone(),
            human_key: definition.node.human_key.clone(),
            title: definition.node.title.clone(),
            status: definition.node.status.clone(),
            view: sources.after.into(),
            source_kind: definition.kind.into(),
            path: definition.path.clone(),
            source_hash: definition.hash.clone(),
            definition_error: None,
        };
        if let Some(existing) = result.intents.iter_mut().find(|intent| {
            intent.id == row.id
                || (intent.definition_error.is_some() && intent.human_key == row.human_key)
        }) {
            *existing = row;
        } else {
            result.intents.push(row);
        }
    }
    let mut seen = BTreeSet::new();
    for (reference, changes) in plans {
        let old: Vec<_> = before_definitions
            .iter()
            .filter(|definition| matches(&definition.node, &reference))
            .collect();
        let new: Vec<_> = after_definitions
            .iter()
            .filter(|definition| matches(&definition.node, &reference))
            .collect();
        if old.len() > 1 || new.len() > 1 {
            result.diagnostics.push(format!(
                "{reference}: ambiguous recorded intent definitions; no criteria guessed"
            ));
            continue;
        }
        let old = old.first().copied();
        let new = new.first().copied();
        let scoped_current = current_definitions
            .iter()
            .find(|definition| matches(&definition.node, &reference));
        let current = if old.is_none() && new.is_none() {
            if let Some(definition) = scoped_current {
                Some(definition.clone())
            } else {
                match bridge::read_intent(repo, &reference).and_then(|inputs| {
                    bridge::lift(&inputs).map(|node| (node, bridge::source_content_hash(&inputs)))
                }) {
                    Ok((node, hash)) => {
                        result
                            .current_sources
                            .insert(reference.clone(), hash.clone());
                        result.diagnostics.push(format!("{reference}: recorded intent source unavailable; current vault definition is labelled context only, not historical acceptance evidence"));
                        Some(Definition {
                            node,
                            hash,
                            kind: "current_vault_context",
                            view: None,
                            path: reference.clone(),
                        })
                    }
                    Err(error) => {
                        result.diagnostics.push(format!(
                            "{reference}: governing intent definition unavailable: {error}"
                        ));
                        None
                    }
                }
            }
        } else {
            None
        };
        let ids: BTreeSet<_> = old
            .into_iter()
            .chain(new)
            .chain(current.as_ref())
            .flat_map(|definition| {
                definition
                    .node
                    .has_acceptance_criterion
                    .iter()
                    .map(|criterion| criterion.id.clone())
            })
            .collect();
        if ids.is_empty() {
            result.diagnostics.push(format!(
                "{reference}: no acceptance criteria available in this intent definition"
            ));
        }
        for id in ids {
            if !seen.insert(id.clone()) {
                let existing = result
                    .criteria
                    .iter_mut()
                    .find(|criterion| criterion.id == id)
                    .unwrap();
                if existing.intent != new.or(old).or(current.as_ref()).unwrap().node.id {
                    return Err(CliError::InvalidArgument {
                        message: format!(
                            "Ambiguous criterion identity {id} in comparison governance"
                        ),
                    });
                }
                existing.governing_changes = existing
                    .governing_changes
                    .iter()
                    .chain(&changes)
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                continue;
            }
            result.criteria.push(ComparedCriterion {
                id: id.clone(),
                intent: new.or(old).or(current.as_ref()).unwrap().node.id.clone(),
                governing_changes: changes.iter().cloned().collect(),
                before: old.and_then(|definition| declaration(definition, &id, &sources.files)),
                after: new.and_then(|definition| declaration(definition, &id, &sources.files)),
                current_context: current
                    .as_ref()
                    .and_then(|definition| declaration(definition, &id, &sources.files)),
            });
        }
    }
    result.criteria.sort_by(|a, b| a.id.cmp(&b.id));
    // Native change context resolves display identities through the live vault.
    // Do not let a later title edit overwrite identity context for a definition
    // we actually have pinned in one of the recorded views.
    for change in &mut result.changes {
        for field in ["plans", "associated_intents"] {
            for item in change.intent_context[field]
                .as_array_mut()
                .into_iter()
                .flatten()
            {
                let Some(identity) = item["intent"].as_object_mut() else {
                    continue;
                };
                let reference = identity
                    .get("uid")
                    .filter(|value| value.as_str().is_some_and(|value| !value.is_empty()))
                    .or_else(|| identity.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let preferred: Vec<_> = after_definitions
                    .iter()
                    .filter(|definition| matches(&definition.node, reference))
                    .collect();
                let preferred = if preferred.is_empty() {
                    before_definitions
                        .iter()
                        .filter(|definition| matches(&definition.node, reference))
                        .collect()
                } else {
                    preferred
                };
                if let [definition] = preferred.as_slice() {
                    identity.insert("title".into(), json!(definition.node.title));
                    identity.insert("source_kind".into(), json!("recorded_view"));
                    identity.insert("source_view".into(), json!(definition.view));
                    identity.insert("source_hash".into(), json!(definition.hash));
                } else {
                    identity.insert("source_kind".into(), json!("current_vault_context"));
                }
            }
        }
    }
    result
        .intents
        .sort_by(|a, b| a.human_key.cmp(&b.human_key).then(a.id.cmp(&b.id)));
    result.diagnostics.sort();
    result.diagnostics.dedup();
    Ok(result)
}

impl Conformance {
    pub(crate) fn reviews(&self) -> Vec<CriterionReview> {
        self.criteria
            .iter()
            .map(|criterion| {
                let declaration = criterion
                    .after
                    .as_ref()
                    .or(criterion.before.as_ref())
                    .or(criterion.current_context.as_ref())
                    .unwrap();
                CriterionReview {
                    id: criterion.id.clone(),
                    intent: criterion.intent.clone(),
                    text: declaration.text.clone(),
                    declared_status: declaration.status.clone(),
                    review_assessment: CriterionOutcome::Unknown,
                    findings: vec![],
                    source_hash: declaration.source_hash.clone(),
                }
            })
            .collect()
    }
    pub(crate) fn verify(&self, repo: &Repository) -> CliResult<()> {
        if let Some(scope) = &self.scope {
            if current_scope(repo, &scope.view)?.hash != scope.current_inventory_hash {
                return Err(CliError::InvalidArgument {message:"View-scoped intent catalogue changed during review/export; retry without stale context".into()});
            }
        }
        for (reference, hash) in &self.current_sources {
            let inputs = bridge::read_intent(repo, reference).map_err(CliError::Repository)?;
            if bridge::source_content_hash(&inputs) != *hash {
                return Err(CliError::InvalidArgument {
                    message:
                        "Current-vault conformance context changed during export; retry review"
                            .into(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_repository::{IntentCreateOptions, IntentUpdateOptions};

    fn create(repo: &Repository, title: &str) -> atomic_repository::IntentCreateResult {
        repo.vault_intent_create(IntentCreateOptions {
            title: title.into(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: None,
            turn_id: None,
            kind: None,
        })
        .unwrap()
    }
    fn pin(repo: &Repository, view: &str) -> Conformance {
        Conformance {
            scope: Some(IntentScope {
                view: view.into(),
                current_inventory_hash: current_scope(repo, view).unwrap().hash,
            }),
            ..Default::default()
        }
    }
    #[test]
    fn scope_inventory_detects_new_intents_and_definition_drift_but_not_unrelated_views() {
        let dir = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        let view = repo.current_view().to_owned();
        let first = pin(&repo, &view);
        repo.create_draft_view("other", &view).unwrap();
        repo.switch_view(wc, "other").unwrap();
        create(&repo, "Unrelated scope");
        first.verify(&repo).unwrap();
        repo.switch_view(wc, &view).unwrap();
        let work = create(&repo, "New view-scoped intent");
        assert!(first.verify(&repo).is_err());
        let second = pin(&repo, &view);
        second.verify(&repo).unwrap();
        repo.vault_intent_update(
            &work.id,
            IntentUpdateOptions {
                content: Some(":::why\nChanged after selection.\n:::\n".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(second.verify(&repo).is_err());
    }
}
