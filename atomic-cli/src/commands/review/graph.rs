//! Three-pass, report-local graph over exact inspection inputs. Never changes ACs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path};

use atomic_canonical::review_finding::{
    AnalysisReport, CriterionOutcome, ReviewFinding, SourceSide,
};
use atomic_canonical::review_graph::*;
use atomic_canonical::CanonicalNode;
use atomic_core::{
    kg::iri,
    types::{Base32, Hash},
};
use atomic_repository::Repository;
use atomic_semantic::ParserRegistry;
use serde_json::{json, Value};

use super::incremental::{ChangeInspection, IncrementalReport};
use crate::error::{CliError, CliResult};

fn invalid(message: impl Into<String>) -> CliError {
    CliError::InvalidArgument {
        message: message.into(),
    }
}

fn encode(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("review graph types are serializable")
}

fn local_id(kind: &str, value: &impl serde::Serialize) -> String {
    format!(
        "{kind}:{}",
        Hash::of(&serde_json::to_vec(value).expect("serializable graph identity")).to_base32()
    )
}

pub(super) fn finding_id(finding: &ReviewFinding) -> String {
    let mut finding = finding.clone();
    finding.acceptance_criteria.clear();
    finding.locations.sort_by_key(|location| {
        (
            location.path.clone(),
            format!("{:?}", location.side),
            location.start_line,
            location.end_line,
        )
    });
    finding
        .evidence
        .sort_by_key(|evidence| serde_json::to_string(evidence).unwrap());
    local_id("finding", &serde_json::to_string(&finding).unwrap())
}

pub(super) fn membership_hash(repo: &Repository, view: &str) -> CliResult<String> {
    let membership: Vec<_> = repo
        .effective_history(Some(view))
        .map_err(CliError::Repository)?
        .into_iter()
        .map(|entry| entry.hash.to_base32())
        .collect();
    Ok(Hash::of(&serde_json::to_vec(&membership).expect("serializable membership")).to_base32())
}

struct IntentCriteria {
    node: CanonicalNode,
    aliases: BTreeSet<String>,
    source_hash: String,
}

fn governed(change: &ChangeInspection, intent: &IntentCriteria) -> bool {
    change.intent_context["status"] == "recorded"
        && change.intent_context["plans"]
            .as_array()
            .is_some_and(|plans| {
                plans.iter().any(|plan| {
                    [
                        plan["plan_id"].as_str(),
                        plan["intent"]["uid"].as_str(),
                        plan["intent"]["id"].as_str(),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|alias| intent.aliases.contains(alias))
                })
            })
}

fn catalog(report: &IncrementalReport) -> Vec<IntentCriteria> {
    report
        .intents
        .iter()
        .filter_map(|document| {
            let node: CanonicalNode =
                serde_json::from_value(document["current_document_criteria_and_tasks"].clone())
                    .ok()?;
            Some(IntentCriteria {
                aliases: [
                    Some(node.id.clone()),
                    Some(node.human_key.clone()),
                    document["id"].as_str().map(str::to_owned),
                    document["frontmatter"]["uid"].as_str().map(str::to_owned),
                ]
                .into_iter()
                .flatten()
                .collect(),
                node,
                source_hash: document["source_hash"].as_str().unwrap_or_default().into(),
            })
        })
        .filter(|intent| {
            report
                .changes
                .iter()
                .any(|change| change.selected_root && governed(change, intent))
                || (report.selection.kind == "intent"
                    && report
                        .selection
                        .value
                        .as_ref()
                        .is_some_and(|id| intent.aliases.contains(id)))
        })
        .collect()
}

pub(super) fn build(
    repo: &Repository,
    report: &IncrementalReport,
    input: Option<&Path>,
    risk_profile: Option<&Path>,
) -> CliResult<ReviewGraph> {
    let profile = if let Some(path) = risk_profile {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            return Err(invalid("risk profile exceeds 64 KiB"));
        }
        serde_json::from_slice::<atomic_audit::risk::RiskProfile>(&bytes)
            .map_err(|error| invalid(format!("invalid risk profile: {error}")))?
    } else {
        atomic_audit::risk::RiskProfile::default()
    };
    profile.validate().map_err(invalid)?;
    let intents = catalog(report);
    let mut criterion_ids = BTreeSet::new();
    for intent in &intents {
        for criterion in &intent.node.has_acceptance_criterion {
            if !criterion_ids.insert(&criterion.id) {
                return Err(invalid("ambiguous duplicate criterion ID in review scope"));
            }
        }
    }
    let scope = ReviewScope {
        project: repo.project_id().map_err(CliError::Repository)?.to_urn(),
        view: report.source["view"].as_str().unwrap_or_default().into(),
        view_state: report.source["view_state"]
            .as_str()
            .unwrap_or_default()
            .into(),
        selected_roots: report.selected_roots.clone(),
        dependency_closure: report.dependency_closure.clone(),
        source_membership_hash: membership_hash(
            repo,
            report.source["view"].as_str().unwrap_or_default(),
        )?,
        intent_sources: intents
            .iter()
            .map(|intent| (intent.node.id.clone(), intent.source_hash.clone()))
            .collect(),
    };
    if repo
        .get_view_info(&scope.view)
        .map_err(CliError::Repository)?
        .state
        .to_base32()
        != scope.view_state
    {
        return Err(invalid(
            "review source changed while resolving graph scope; retry inspection",
        ));
    }
    let mut observations: Vec<_> = report
        .analyses()
        .flat_map(|(_, analysis)| {
            analysis
                .findings
                .iter()
                .cloned()
                .map(|finding| SubmittedFinding {
                    pass: ReviewPass::AntiSlop,
                    finding,
                })
        })
        .collect();
    let mut risk_assessment = None;
    if let Some(path) = input {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(invalid("review findings input exceeds 1 MiB"));
        }
        let mut imported: FindingsInput = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(format!("invalid review findings input: {error}")))?;
        if imported.schema != INPUT_SCHEMA || imported.scope != scope {
            return Err(invalid("review findings scope/schema is stale or differs from the selected roots, source view or intent revisions"));
        }
        if imported.findings.len() > 500 {
            return Err(invalid("review findings input exceeds 500 findings"));
        }
        for observation in &imported.findings {
            validate_input(repo, report, &intents, observation)?;
        }
        risk_assessment = imported.risk_assessment.take();
        observations.append(&mut imported.findings);
    }

    let (impact_reports, mut contract_findings) = if let Some(comparison) = &report.view_comparison
    {
        super::view_comparison::inspect_impact(repo, comparison)
    } else if report.view_snapshot.is_some() {
        (vec![], vec![])
    } else {
        super::impact::inspect(repo, report)
    };
    let impact_comparison_states: BTreeMap<String, FindingComparison> =
        if report.view_comparison.is_some() {
            impact_reports
                .iter()
                .flat_map(|report| &report.issues)
                .zip(&contract_findings)
                .map(|(issue, submitted)| {
                    (
                        finding_id(&submitted.finding),
                        super::impact::comparison_state(issue),
                    )
                })
                .collect()
        } else {
            BTreeMap::new()
        };
    observations.append(&mut contract_findings);

    // Identical observations across passes have one number. Criterion judgments
    // can enrich an existing observation without creating another copy of it.
    let mut unique = BTreeMap::<String, (ReviewFinding, BTreeSet<ReviewPass>)>::new();
    for observation in observations {
        let mut finding = observation.finding;
        finding.locations.sort_by_key(|location| {
            (
                location.path.clone(),
                format!("{:?}", location.side),
                location.start_line,
                location.end_line,
            )
        });
        finding
            .evidence
            .sort_by_key(|evidence| serde_json::to_string(evidence).unwrap());
        let mut identity = finding.clone();
        identity.acceptance_criteria.clear();
        let key = serde_json::to_string(&identity).unwrap();
        let (existing, passes) = unique
            .entry(key)
            .or_insert_with(|| (identity, BTreeSet::new()));
        passes.insert(observation.pass);
        for link in finding.acceptance_criteria {
            if let Some(previous) = existing
                .acceptance_criteria
                .iter()
                .find(|old| old.criterion == link.criterion)
            {
                if previous != &link {
                    return Err(invalid(
                        "conflicting assessments for the same finding and criterion",
                    ));
                }
            } else {
                existing.acceptance_criteria.push(link);
            }
        }
        existing
            .acceptance_criteria
            .sort_by(|a, b| a.criterion.cmp(&b.criterion));
    }
    let mut findings = Vec::new();
    let comparison_states: BTreeMap<_, _> = report
        .view_comparison
        .iter()
        .flat_map(|comparison| comparison.files.iter())
        .flat_map(|file| file.finding_states.iter())
        .collect();
    for (index, (identity, (finding, passes))) in unique.into_iter().enumerate() {
        let change = report
            .changes
            .iter()
            .find(|change| change.hash == finding.change);
        let related_criteria = if let Some(change) = change {
            intents
                .iter()
                .filter(|intent| governed(change, intent))
                .flat_map(|intent| intent.node.has_task.iter())
                .filter(|task| {
                    task.touches_file.iter().any(|path| {
                        finding
                            .locations
                            .iter()
                            .any(|location| location.path == *path)
                    })
                })
                .flat_map(|task| task.satisfies.as_slice().iter().cloned())
                .filter(|id| {
                    intents.iter().any(|intent| {
                        intent
                            .node
                            .has_acceptance_criterion
                            .iter()
                            .any(|criterion| criterion.id == *id)
                    })
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else if let Some(conformance) = report.conformance() {
            conformance
                .criteria
                .iter()
                .filter(|criterion| {
                    criterion
                        .before
                        .iter()
                        .chain(&criterion.after)
                        .chain(&criterion.current_context)
                        .flat_map(|declaration| &declaration.tasks)
                        .flat_map(|task| &task.files)
                        .any(|file| {
                            finding.locations.iter().any(|location| {
                                file["path"].as_str() == Some(location.path.as_str())
                            })
                        })
                })
                .map(|criterion| criterion.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        let id = local_id("finding", &identity);
        let comparison_state = comparison_states
            .get(&id)
            .map(|state| (*state).clone())
            .or_else(|| impact_comparison_states.get(&id).cloned());
        findings.push(NumberedFinding {
            number: index as u32 + 1,
            id: local_id("finding", &identity),
            passes: passes.into_iter().collect(),
            finding,
            related_criteria,
            comparison: comparison_state,
        });
    }
    let mut criteria = BTreeMap::new();
    for intent in &intents {
        for criterion in &intent.node.has_acceptance_criterion {
            let numbers: Vec<_> = findings
                .iter()
                .filter(|finding| {
                    finding.finding.acceptance_criteria.iter().any(|link| {
                        link.criterion == criterion.id && link.assessment == CriterionOutcome::Unmet
                    })
                })
                .map(|finding| finding.number)
                .collect();
            criteria.insert(
                criterion.id.clone(),
                CriterionReview {
                    id: criterion.id.clone(),
                    intent: intent.node.id.clone(),
                    text: criterion.text.clone(),
                    declared_status: criterion.ac_status.clone(),
                    review_assessment: if numbers.is_empty() {
                        CriterionOutcome::Unknown
                    } else {
                        CriterionOutcome::Unmet
                    },
                    findings: numbers,
                    source_hash: intent.source_hash.clone(),
                },
            );
        }
    }
    let has_assessments = findings
        .iter()
        .any(|finding| !finding.finding.acceptance_criteria.is_empty());
    if let Some(conformance) = report.conformance() {
        for criterion in conformance.reviews() {
            criteria.insert(criterion.id.clone(), criterion);
        }
    }
    let mut graph = ReviewGraph {
        schema: SCHEMA.into(), scope,
        passes: vec![
            PassResult { pass: ReviewPass::AntiSlop, status: PassStatus::Partial, detail: "Executable audit checks over recorded before/after source; supported patterns and language coverage are explicit, not whole-program behavioral proof".into() },
            PassResult { pass: ReviewPass::IntentConformance, status: if criteria.is_empty() { PassStatus::Blocked } else if !has_assessments { PassStatus::Pending } else { PassStatus::Partial }, detail: if criteria.is_empty() { "No unambiguous governing acceptance criteria were resolved; conformance is not evaluated. File associations alone do not establish ownership." } else { "Criterion assessments come from explicit supplied findings, not author status, shared paths or automatic semantic judgment; no source intent was changed" }.into() },
            PassResult { pass: ReviewPass::BlastRadiusRisk, status: PassStatus::Partial, detail: "Recorded before/after direct-call binding, changed-function fan-out and declared call-contract checks; not whole-program/type/runtime proof".into() },
        ], findings, criteria: criteria.into_values().collect(), nodes: vec![], edges: vec![], diagnostics: vec!["Report-local judgments are not signed approval. Current intent definitions are source-hashed context, not authenticated historical criteria.".into()], risk: None,
    };
    let code_changes: Vec<_> = report
        .changes
        .iter()
        .filter(|change| {
            change.selected_root
                && change
                    .paths
                    .iter()
                    .any(|path| !atomic_repository::is_vault_path(path))
        })
        .collect();
    let metadata_only = !report.selected_roots.is_empty()
        && report
            .changes
            .iter()
            .filter(|change| change.selected_root)
            .any(|change| !change.paths.is_empty())
        && code_changes.is_empty();
    if metadata_only {
        for (index, pass, detail) in [
            (0, ReviewPass::AntiSlop, "No code to analyze: selected paths are .vault metadata; code audits and metrics were not run."),
            (2, ReviewPass::BlastRadiusRisk, "No code changed: .vault metadata excluded; no project scan was started."),
        ] {
            graph.passes[index].status = if graph.findings.iter().any(|finding| finding.passes.contains(&pass)) { PassStatus::Partial } else { PassStatus::NotApplicable };
            graph.passes[index].detail = detail.into();
        }
    }
    if let Some(snapshot) = &report.view_snapshot {
        graph.passes[0].detail = "Local audits over each live recorded view file once; no historical replay, before baseline or introduction attribution. Language and resource gaps remain explicit.".into();
        if snapshot
            .paths
            .iter()
            .all(|path| atomic_repository::is_vault_path(path))
        {
            graph.passes[0].status = PassStatus::NotApplicable;
            graph.passes[0].detail =
                "No code to analyze: the recorded view is empty or contains only .vault metadata."
                    .into();
        }
        graph.passes[1].status = if snapshot
            .conformance
            .as_ref()
            .is_some_and(|context| !context.intents.is_empty() || !context.criteria.is_empty())
        {
            PassStatus::Partial
        } else {
            PassStatus::Blocked
        };
        graph.passes[1].detail =
            "View-assigned intents and their declared acceptance criteria are review context, independent of change-plan links. Recorded definitions are preferred; current-vault definitions are labelled context. View assignment is not governing-change attribution or an acceptance verdict."
                .into();
        if let Some(context) = &snapshot.conformance {
            graph
                .diagnostics
                .extend(context.diagnostics.iter().cloned());
        }
        graph.passes[2].status = PassStatus::Pending;
        graph.passes[2].detail = "Snapshot metrics include declared public surface and dependency fan-out. Change-regression risk and before/after call-contract checks have no baseline in this mode.".into();
    }
    if let Some(comparison) = &report.view_comparison {
        graph.passes[0].detail="Local audit observations from both recorded view states, classified as new/existing/resolved/changed/unknown with explicit source coverage. These are not authored-change or behavioral-proof claims.".into();
        if comparison
            .files
            .iter()
            .all(|file| file.analysis.checks.is_empty())
        {
            graph.passes[0].status = PassStatus::NotApplicable;
        }
        graph.passes[1].status =
            if !comparison.conformance.intents.is_empty() || !graph.criteria.is_empty() {
                PassStatus::Partial
            } else if comparison.conformance.changes.is_empty() {
                PassStatus::NotApplicable
            } else {
                PassStatus::Blocked
            };
        graph.passes[1].detail="Intents explicitly assigned to the From/source view and their acceptance criteria are included independently of recorded change-plan links. Actual membership-delta governance remains separate context. Recorded definitions are preferred; current-vault fallback is labelled context only. Scope, author claims and file overlap do not establish an independent acceptance verdict.".into();
        graph
            .diagnostics
            .extend(comparison.conformance.diagnostics.iter().cloned());
        let paths = super::view_comparison::changed_paths(comparison);
        graph.passes[2].status = if paths.is_empty() {
            PassStatus::NotApplicable
        } else {
            PassStatus::Partial
        };
        graph.passes[2].detail="Shared semantic analysis of the pinned From/To states: changed functions, before/after bound callers, direct-call contracts and local risk estimate. Source-selection gaps are explicit; compiler/type/runtime behavior is not proven.".into();
    }
    if !impact_reports.is_empty() {
        graph.diagnostics.push(if report.view_comparison.is_some() { "Direct impact compares the two named effective recorded snapshots. Unchanged sources are caller context, not additional changed roots; no patch-prefix replay or dirty-source fallback." } else { "Direct impact uses recorded before/after view-prefix snapshots for each selected change; other files are caller context only, never later or dirty source." }.into());
    }
    project(repo, report, &mut graph);
    super::impact::project(&mut graph, &impact_reports);
    if report.view_snapshot.is_some() {
        super::risk::attach_snapshot(&mut graph);
    } else {
        super::risk::attach(&mut graph, risk_assessment, impact_reports, &profile)
            .map_err(invalid)?;
    }
    if membership_hash(repo, &graph.scope.view)? != graph.scope.source_membership_hash
        || repo
            .get_view_info(&graph.scope.view)
            .map_err(CliError::Repository)?
            .state
            .to_base32()
            != graph.scope.view_state
    {
        return Err(invalid("review source membership changed during graph inspection; retry without reusing stale findings"));
    }
    for intent in &intents {
        let inputs = atomic_repository::intent_bridge::read_intent(repo, &intent.node.human_key)
            .map_err(CliError::Repository)?;
        if atomic_repository::intent_bridge::source_content_hash(&inputs) != intent.source_hash {
            return Err(invalid("review criterion source changed during graph inspection; retry without reusing stale judgments"));
        }
    }
    graph.diagnostics.sort();
    graph.diagnostics.dedup();
    Ok(graph)
}

fn validate_input(
    repo: &Repository,
    report: &IncrementalReport,
    intents: &[IntentCriteria],
    observation: &SubmittedFinding,
) -> CliResult<()> {
    let finding = &observation.finding;
    finding.validate().map_err(invalid)?;
    let change = report
        .changes
        .iter()
        .find(|change| change.selected_root && change.hash == finding.change)
        .ok_or_else(|| invalid("finding is not about an exact selected root"))?;
    if observation.pass == ReviewPass::IntentConformance && finding.acceptance_criteria.is_empty() {
        return Err(invalid(
            "intent-conformance finding requires an explicit acceptance-criterion assessment",
        ));
    }
    let mut linked = BTreeSet::new();
    for link in &finding.acceptance_criteria {
        if !linked.insert(&link.criterion) {
            return Err(invalid("duplicate criterion assessment"));
        }
        if !intents
            .iter()
            .filter(|intent| governed(change, intent))
            .any(|intent| {
                intent
                    .node
                    .has_acceptance_criterion
                    .iter()
                    .any(|criterion| criterion.id == link.criterion)
            })
        {
            return Err(invalid("finding criterion is missing or does not belong to this change's unambiguous governing intent"));
        }
    }
    let hash = Hash::from_base32(finding.change.as_bytes())
        .ok_or_else(|| invalid("invalid finding change"))?;
    for location in &finding.locations {
        if location.path.is_empty()
            || Path::new(&location.path)
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(invalid(
                "finding requires a repository-relative recorded path",
            ));
        }
        let bytes = match location.side {
            SourceSide::Before => repo.get_file_content_before_change(&location.path, &hash),
            SourceSide::After => repo.get_file_content_after_change(&location.path, &hash),
        }
        .map_err(CliError::Repository)?
        .ok_or_else(|| invalid("finding location has no recorded content on the requested side"))?;
        let source = std::str::from_utf8(&bytes)
            .map_err(|_| invalid("finding source location is not UTF-8 text"))?;
        if location.end_line as usize > source.split('\n').count() {
            return Err(invalid(
                "finding location exceeds the recorded source line range",
            ));
        }
    }
    Ok(())
}

#[derive(Default)]
struct Projection {
    nodes: BTreeMap<String, GraphNode>,
    edges: BTreeSet<GraphEdge>,
}
impl Projection {
    fn node(&mut self, id: &str, kind: &str, label: &str, properties: Value) {
        self.nodes.entry(id.into()).or_insert_with(|| GraphNode {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            properties,
        });
    }
    fn edge(&mut self, from: &str, relation: &str, to: &str) {
        self.edges.insert(GraphEdge {
            from: from.into(),
            relation: relation.into(),
            to: to.into(),
        });
    }
}

fn project(repo: &Repository, report: &IncrementalReport, graph: &mut ReviewGraph) {
    let mut out = Projection::default();
    let project = repo.project_id().expect("resolved project");
    out.node(
        "review",
        "review",
        "Three-pass review (not independent approval)",
        encode(&graph.scope),
    );
    let excluded: BTreeSet<_> = report
        .analyses()
        .flat_map(|(_, analysis)| analysis.excluded_metadata_paths.iter())
        .collect();
    if !excluded.is_empty() {
        out.node(
            "excluded-vault",
            "analysis_exclusion",
            ".vault metadata excluded from code analysis (retained as evidence)",
            json!({"paths": excluded}),
        );
        out.edge("review", "excludes_from_code_analysis", "excluded-vault");
    }
    for pass in &graph.passes {
        out.node(
            pass.pass.id(),
            "review_pass",
            pass.pass.label(),
            encode(pass),
        );
        out.edge("review", "has_pass", pass.pass.id());
    }
    for change in report.changes.iter().filter(|change| change.selected_root) {
        let id =
            iri::mint_change(&Hash::from_base32(change.hash.as_bytes()).expect("resolved hash"));
        out.node(&id, "change", &change.hash, json!({"hash": change.hash}));
        out.edge("review", "reviews", &id);
        project_checks(&mut out, &change.analysis);
        project_metrics(&mut out, &change.hash, &change.analysis, &project, None);
    }
    if let Some(snapshot) = &report.view_snapshot {
        out.node(&snapshot.source_ref,"view_snapshot",&format!("Recorded view {}",snapshot.view),json!({"source_ref":snapshot.source_ref,"view":snapshot.view,"state":snapshot.state,"membership_hash":snapshot.membership_hash,"paths":snapshot.paths,"conformance":snapshot.conformance}));
        out.edge("review", "reviews_snapshot", &snapshot.source_ref);
        project_checks(&mut out, &snapshot.analysis);
        project_metrics(
            &mut out,
            &snapshot.source_ref,
            &snapshot.analysis,
            &project,
            Some("view_snapshot"),
        );
    }
    if let Some(comparison) = &report.view_comparison {
        out.node(
            &comparison.source_ref,
            "view_comparison",
            "Paired recorded views",
            json!({"from":comparison.from,"to":comparison.to,"before":comparison.before,"after":comparison.after,"diagnostics":comparison.diagnostics,"conformance":comparison.conformance}),
        );
        out.edge("review", "compares_views", &comparison.source_ref);
        for file in &comparison.files {
            out.node(&file.source_ref,"compared_file",&file.path,json!({"path":file.path,"before_path":file.before_path,"after_path":file.after_path,"status":file.status,"content_status":file.content_status}));
            out.edge(&comparison.source_ref, "compares_file", &file.source_ref);
            project_checks(&mut out, &file.analysis);
            project_metrics(
                &mut out,
                &file.source_ref,
                &file.analysis,
                &project,
                Some("view_comparison"),
            );
        }
    }
    if let Some(conformance) = report.conformance() {
        for intent in &conformance.intents {
            out.node(
                &intent.id,
                "intent",
                &format!("{} · {}", intent.human_key, intent.title),
                encode(intent),
            );
            out.edge(
                ReviewPass::IntentConformance.id(),
                "includes_view_scoped_intent",
                &intent.id,
            );
        }
    }
    for criterion in &graph.criteria {
        out.node(
            &criterion.id,
            "acceptance_criterion",
            &criterion.text,
            encode(criterion),
        );
        out.node(
            &criterion.intent,
            "intent",
            &criterion.intent,
            json!({"source_hash": criterion.source_hash}),
        );
        out.edge(&criterion.intent, "declares", &criterion.id);
        out.edge(
            ReviewPass::IntentConformance.id(),
            "assesses",
            &criterion.id,
        );
    }
    let mut parser = None::<ParserRegistry>;
    for finding in &graph.findings {
        out.node(
            &finding.id,
            "finding",
            &format!("F{}: {}", finding.number, finding.finding.summary),
            encode(finding),
        );
        for pass in &finding.passes {
            out.edge(pass.id(), "reports", &finding.id);
        }
        let rule = format!("rule:{}", finding.finding.requirement_or_rule.id);
        out.node(
            &rule,
            "engineering_rule",
            &finding.finding.requirement_or_rule.explanation,
            encode(&finding.finding.requirement_or_rule),
        );
        out.edge(&finding.id, "concerns_rule", &rule);
        for criterion in &finding.related_criteria {
            out.edge(&finding.id, "related_criterion_not_a_verdict", criterion);
        }
        for link in &finding.finding.acceptance_criteria {
            out.edge(
                &finding.id,
                if link.assessment == CriterionOutcome::Unmet {
                    "assesses_unmet"
                } else {
                    "assessment_unknown"
                },
                &link.criterion,
            );
        }
        for location in &finding.finding.locations {
            let id = local_id("location", &(&finding.finding.change, location));
            out.node(
                &id,
                "code_location",
                &format!(
                    "{}:{}-{} ({:?})",
                    location.path, location.start_line, location.end_line, location.side
                ),
                json!({"change": finding.finding.change, "location": location}),
            );
            out.edge(&finding.id, "located_at", &id);
            let file = iri::mint_file(&project, &location.path);
            out.node(&file, "file", &location.path, json!({"resource": file}));
            out.edge(&id, "in_file", &file);
            // Automatic contract findings already carry pinned semantic symbol
            // identities; impact projection links them without another parse.
            if finding.finding.requirement_or_rule.id
                == "compatibility.resolved-direct-call-contract"
                && finding
                    .finding
                    .analyzer
                    .starts_with("atomic-audit;direct-call-contracts=")
            {
                continue;
            }
            if atomic_repository::is_vault_path(&location.path) {
                continue;
            }
            // Snapshot structure anchors already come from its metrics. Do not
            // re-render and reparse a view file once for each located finding.
            if report.is_view_review() {
                continue;
            }
            let hash = Hash::from_base32(finding.finding.change.as_bytes()).expect("finding hash");
            let bytes = match location.side {
                SourceSide::Before => repo.get_file_content_before_change(&location.path, &hash),
                SourceSide::After => repo.get_file_content_after_change(&location.path, &hash),
            };
            if let Ok(Some(bytes)) = bytes {
                if bytes.len() > atomic_semantic::MAX_SOURCE_BYTES {
                    continue;
                }
                if let Ok(source) = std::str::from_utf8(&bytes) {
                    let entities = parser
                        .get_or_insert_with(ParserRegistry::new)
                        .extract(&location.path, source);
                    if let Some(entity) = entities
                        .iter()
                        .filter(|entity| {
                            entity.line <= location.start_line
                                && entity.end_line >= location.end_line
                        })
                        .min_by_key(|entity| entity.end_line - entity.line)
                    {
                        let entity_id = local_id(
                            "structure",
                            &(
                                &finding.finding.change,
                                location.side.clone(),
                                &entity.file,
                                &entity.name,
                                entity.line,
                            ),
                        );
                        out.node(&entity_id, "code_structure", &format!("{} {}", entity.kind, entity.name), json!({"change": finding.finding.change, "side": location.side, "entity": entity}));
                        out.edge(&id, "within", &entity_id);
                        out.edge(&entity_id, "in_file", &file);
                    }
                }
            }
        }
        for evidence in &finding.finding.evidence {
            let id = local_id("evidence", evidence);
            out.node(&id, "evidence", &evidence.observation, encode(evidence));
            out.edge(&finding.id, "supported_by", &id);
        }
    }
    graph.nodes = out.nodes.into_values().collect();
    graph.edges = out.edges.into_iter().collect();
}

fn project_checks(out: &mut Projection, analysis: &AnalysisReport) {
    for check in &analysis.checks {
        let id = local_id("check", check);
        out.node(
            &id,
            "check",
            &format!("{}: {}", check.rule, check.path),
            encode(check),
        );
        out.edge(ReviewPass::AntiSlop.id(), "has_check", &id);
    }
}

fn measurement_label(value: &atomic_audit::Measurement) -> String {
    match value {
        atomic_audit::Measurement::Measured { value } => value.to_string(),
        atomic_audit::Measurement::Unavailable { reason } => format!("unknown ({reason})"),
        atomic_audit::Measurement::NotApplicable { .. } => "N/A".into(),
    }
}

fn project_metrics(
    out: &mut Projection,
    reference: &str,
    analysis: &AnalysisReport,
    project: &atomic_core::kg::project_id::ProjectId,
    view_mode: Option<&str>,
) {
    use atomic_audit::Metric;
    if analysis.metrics.is_empty() {
        return;
    }
    let scope = local_id("metric-scope", &reference);
    out.node(&scope, "metric_comparison_scope", if view_mode.is_some() { "Recorded view corpus (see view/comparison root; language/subject/resource limits apply)" } else { "Recorded supplied paths only (not a repository-wide clone scan)" }, json!({
        "change": reference, "paths": analysis.metric_comparison_scope, "diagnostics": analysis.metric_diagnostics,
    }));
    for record in &analysis.metrics {
        let metric = record.observation.metric;
        let pass = if matches!(metric, Metric::DependencyFanOut | Metric::PublicSurface) {
            ReviewPass::BlastRadiusRisk
        } else {
            ReviewPass::AntiSlop
        };
        let id = local_id("metric", &(&reference, metric, &record.observation.subject));
        let delta = record
            .delta
            .map(|delta| format!(" (delta {delta:+})"))
            .unwrap_or_default();
        let label = if view_mode == Some("view_snapshot") {
            format!(
                "{} {}: {} (recorded view snapshot)",
                metric.id(),
                record.observation.subject,
                measurement_label(&record.observation.after)
            )
        } else {
            format!(
                "{} {}: {} → {}{delta}",
                metric.id(),
                record.observation.subject,
                measurement_label(&record.observation.before),
                measurement_label(&record.observation.after)
            )
        };
        let mut properties = encode(record);
        properties["change"] = json!(reference);
        if let Some(kind) = view_mode {
            properties["scope_kind"] = json!(kind);
        }
        out.node(&id, "metric", &label, properties);
        out.edge(pass.id(), "measures", &id);
        out.edge(&id, "within_comparison_scope", &scope);
        let card = atomic_audit::METRIC_CARDS
            .iter()
            .find(|card| card.metric == metric)
            .expect("metric card");
        let card_id = format!("metric-card:{}", metric.id());
        out.node(&card_id, "metric_card", card.title, encode(card));
        out.edge(&id, "defined_by", &card_id);
        let (location, side) = if let Some(location) = &record.after_location {
            (Some(location), SourceSide::After)
        } else {
            (record.before_location.as_ref(), SourceSide::Before)
        };
        if let Some(location) = location {
            let location = atomic_canonical::review_finding::Location {
                path: record.path.clone(),
                side,
                start_line: location.start_line,
                end_line: location.end_line,
            };
            let anchor = local_id("location", &(&reference, &location));
            out.node(
                &anchor,
                "code_location",
                &format!(
                    "{}:{}-{} ({:?})",
                    location.path, location.start_line, location.end_line, location.side
                ),
                json!({"change": reference, "location": location}),
            );
            out.edge(&id, "located_at", &anchor);
            let file = iri::mint_file(project, &record.path);
            out.node(&file, "file", &record.path, json!({"resource": file}));
            out.edge(&anchor, "in_file", &file);
            if matches!(
                record.scope,
                atomic_audit::catalog::MetricScope::Function
                    | atomic_audit::catalog::MetricScope::Class
            ) {
                let subject = local_id(
                    "metric-subject",
                    &(&reference, &record.observation.subject, &location.side),
                );
                out.node(
                    &subject,
                    "code_structure",
                    &record.observation.subject,
                    json!({"change": reference, "scope": record.scope, "location": location}),
                );
                out.edge(&id, "measures_structure", &subject);
                out.edge(&subject, "located_at", &anchor);
            }
        }
    }
}

/// Terminal presentation uses the same numbered findings and source anchors.
#[cfg(test)]
pub(super) fn render(graph: &ReviewGraph) -> String {
    render_with_details(graph, false)
}

pub(super) fn render_with_details(graph: &ReviewGraph, details: bool) -> String {
    super::terminal::render(graph, details)
}
