//! Recorded snapshot adapter for pure-source direct-call contract analysis.

use atomic_audit::impact::ImpactReport;
use atomic_canonical::review_finding::{
    Confidence, Evidence, EvidenceKind, Introduction, IntroductionStatus, Location, ReviewFinding,
    RuleReference, Severity, SourceSide,
};
use atomic_canonical::review_graph::{
    FindingComparison, GraphEdge, GraphNode, ReviewGraph, ReviewPass, SubmittedFinding,
};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{repository::RecordedSemanticView, Repository};
use atomic_semantic::ParserRegistry;
use std::collections::BTreeSet;

use super::incremental::IncrementalReport;

pub(super) fn inspect(
    repo: &Repository,
    report: &IncrementalReport,
) -> (Vec<ImpactReport>, Vec<SubmittedFinding>) {
    let (mut results, mut findings) = (Vec::new(), Vec::new());
    let mut parser = ParserRegistry::new();
    for change in report.changes.iter().filter(|change| {
        change.selected_root
            && change
                .paths
                .iter()
                .any(|path| !atomic_repository::is_vault_path(path))
    }) {
        let mut result = ImpactReport {
            change: change.hash.clone(),
            code_changed: true,
            ..Default::default()
        };
        let paths: Vec<_> = change
            .paths
            .iter()
            .filter(|path| !atomic_repository::is_vault_path(path))
            .cloned()
            .collect();
        if !paths.iter().any(|path| atomic_semantic::is_supported(path)) {
            result.gaps.push("No supported source in selected paths; non-code/runtime configuration effects require separate inspection".into());
            results.push(result);
            continue;
        }
        if results.len() >= 4 {
            result.gaps.push(
                "Impact snapshot budget reached (4 selected changes); remaining roots not analyzed"
                    .into(),
            );
            results.push(result);
            continue;
        }
        let hash = Hash::from_base32(change.hash.as_bytes()).expect("resolved selected hash");
        match (
            repo.semantic_view_at_change(&hash, false),
            repo.semantic_view_at_change(&hash, true),
        ) {
            (Ok(before_view), Ok(after_view)) => {
                result = inspect_views(&before_view, &after_view, &paths, &mut parser);
                result.change = hash.to_base32();
            }
            (before, after) => {
                if let Err(error) = before {
                    result
                        .gaps
                        .push(format!("Before snapshot unavailable: {error}"));
                }
                if let Err(error) = after {
                    result
                        .gaps
                        .push(format!("After snapshot unavailable: {error}"));
                }
            }
        }
        findings.extend(contract_findings(&result));
        results.push(result);
    }
    (results, findings)
}

pub(super) fn inspect_views(
    before_view: &RecordedSemanticView<'_>,
    after_view: &RecordedSemanticView<'_>,
    paths: &[String],
    parser: &mut ParserRegistry,
) -> ImpactReport {
    let before = before_view.changed_sources(paths, parser);
    let after = after_view.changed_sources(paths, parser);
    let callable = |kind: atomic_semantic::EntityKind| {
        matches!(
            kind,
            atomic_semantic::EntityKind::Function | atomic_semantic::EntityKind::Method
        )
    };
    let old: std::collections::BTreeMap<_, _> = before
        .files
        .iter()
        .flat_map(|file| &file.symbols)
        .filter(|symbol| callable(symbol.entity.kind))
        .map(|symbol| (&symbol.id, symbol))
        .collect();
    let new: std::collections::BTreeMap<_, _> = after
        .files
        .iter()
        .flat_map(|file| &file.symbols)
        .filter(|symbol| callable(symbol.entity.kind))
        .map(|symbol| (&symbol.id, symbol))
        .collect();
    let targets: Vec<_> = old
        .values()
        .chain(new.values())
        .filter(|symbol| {
            !old.get(&symbol.id)
                .zip(new.get(&symbol.id))
                .is_some_and(|(a, b)| {
                    a.tokens == b.tokens
                        && a.public == b.public
                        && a.restricted == b.restricted
                        && a.parent_visible == b.parent_visible
                        && a.conditional == b.conditional
                })
        })
        .map(|symbol| (*symbol).clone())
        .collect();
    match (
        before_view.select_context(before, &targets, parser),
        after_view.select_context(after, &targets, parser),
    ) {
        (Ok(before), Ok(after)) => atomic_audit::impact::analyze_semantic(before, after, paths),
        (before, after) => {
            let mut result = ImpactReport {
                code_changed: true,
                ..Default::default()
            };
            if let Err(error) = before {
                result
                    .gaps
                    .push(format!("Before semantic selection unavailable: {error}"));
            }
            if let Err(error) = after {
                result
                    .gaps
                    .push(format!("After semantic selection unavailable: {error}"));
            }
            result
        }
    }
}

pub(super) fn contract_findings(result: &ImpactReport) -> Vec<SubmittedFinding> {
    let mut findings = Vec::new();
    let reference = &result.change;
    let comparison = reference.starts_with("urn:atomic:view-comparison:");
    for issue in &result.issues {
        let location = &issue.primary_location;
        let side = if issue.primary_before {
            SourceSide::Before
        } else {
            SourceSide::After
        };
        findings.push(SubmittedFinding { pass: ReviewPass::BlastRadiusRisk, finding: ReviewFinding {
                analyzer: format!("atomic-audit;direct-call-contracts=v1;{}", atomic_audit::VERSION),
                change: reference.clone(), summary: issue.summary.clone(),
                locations: vec![Location { path: location.path.clone(), side, start_line: location.start_line, end_line: location.end_line }],
                requirement_or_rule: RuleReference { id: "compatibility.resolved-direct-call-contract".into(), explanation: "Supported direct references must satisfy declaration availability, declared visibility/export access, and fixed/optional/rest argument-count contracts.".into() },
                evidence: vec![
                    Evidence { kind: EvidenceKind::Source, reference: format!("{}:after:{}:{}-{}", reference, issue.call.location.path, issue.call.location.start_line, issue.call.location.end_line), observation: format!("Caller {} targets {}. {}", issue.call.caller, issue.function, issue.evidence) },
                    Evidence { kind: EvidenceKind::Diff, reference: issue.before_declaration.as_ref().map(|location| format!("{}:before:{}:{}-{}", reference, location.path, location.start_line, location.end_line)).unwrap_or_else(|| format!("{}:before", reference)), observation: if comparison { "Static declaration and call-expression evidence between the pinned From/To view states; not attribution to an authored change." } else if issue.introduced { "Recorded binding/call-expression comparison establishes an introduced incompatible call or a surviving previously compatible call broken by the selected declaration change." } else { "An after-side incompatibility is established, but baseline coverage/binding does not establish introduction by this change." }.into() },
                ],
                introduced_by_change: Introduction { status: if issue.introduced && !comparison { IntroductionStatus::Yes } else { IntroductionStatus::Unknown }, rationale: if comparison { "Compared recorded view states establish static contract evidence, not introduction by one authored change." } else if issue.introduced { "Recorded before/after declaration and call-expression comparison attributes the new incompatible direct call to the selected change." } else { "After-side binding/arity is incompatible, but introduction is not established by the available baseline." }.into() },
                severity: Severity::High, confidence: if issue.call.configuration_dependent { Confidence::Medium } else { Confidence::High },
                proposed_correction: "Update the affected consumer and declaration together, or preserve a compatible adapter; add a focused regression test for the recorded call contract.".into(), acceptance_criteria: vec![],
            }});
    }
    findings
}

pub(super) fn comparison_state(issue: &atomic_audit::impact::ContractIssue) -> FindingComparison {
    FindingComparison {
        status: if issue.introduced { "new" } else { "unknown" }.into(),
        before: issue.before_declaration.iter().map(|location|Location {path:location.path.clone(),side:SourceSide::Before,start_line:location.start_line,end_line:location.end_line}).collect(),
        after: vec![Location {path:issue.call.location.path.clone(),side:SourceSide::After,start_line:issue.call.location.start_line,end_line:issue.call.location.end_line}],
        rationale:"Static call-contract evidence between the two pinned view states; not introduction by one authored change or whole-program proof.".into(),
    }
}

pub(super) fn project(graph: &mut ReviewGraph, reports: &[ImpactReport]) {
    let mut present: BTreeSet<_> = graph.nodes.iter().map(|node| node.id.clone()).collect();
    for report in reports {
        let id = format!("direct-impact:{}", report.change);
        graph.nodes.push(GraphNode {
            id: id.clone(),
            kind: "direct_impact_analysis".into(),
            label: "Recorded before/after direct-call and contract evidence".into(),
            properties: serde_json::to_value(report).expect("impact report serializes"),
        });
        graph.edges.push(GraphEdge {
            from: ReviewPass::BlastRadiusRisk.id().into(),
            relation: "analyzes_direct_impact".into(),
            to: id,
        });
        for (side, calls) in [
            ("before", &report.callers_before),
            ("after", &report.callers_after),
            ("after", &report.callees_after),
        ] {
            for call in calls {
                let from = format!("bound-function:{}:{side}:{}", report.change, call.caller);
                let to = format!("bound-function:{}:{side}:{}", report.change, call.callee);
                for (id, name) in [(&from, &call.caller), (&to, &call.callee)] {
                    if present.insert(id.clone()) {
                        graph.nodes.push(GraphNode {
                            id: id.clone(),
                            kind: "bound_function".into(),
                            label: name.clone(),
                            properties: serde_json::json!({"change": report.change, "side": side}),
                        });
                    }
                }
                graph.edges.push(GraphEdge {
                    from,
                    relation: if !call.target_present {
                        "surviving_reference_to_removed_function"
                    } else if !call.visibility_compatible {
                        "inaccessible_direct_reference"
                    } else {
                        "statically_bound_direct_call"
                    }
                    .into(),
                    to,
                });
            }
        }
        for finding in graph.findings.iter().filter(|finding| {
            finding.finding.change == report.change
                && finding.finding.requirement_or_rule.id
                    == "compatibility.resolved-direct-call-contract"
        }) {
            for issue in report.issues.iter().filter(|issue| {
                issue.summary == finding.finding.summary
                    && finding.finding.locations.iter().any(|location| {
                        location.path == issue.primary_location.path
                            && location.start_line == issue.primary_location.start_line
                            && location.end_line == issue.primary_location.end_line
                    })
            }) {
                let symbol =
                    if issue.primary_location == issue.call.location && !issue.primary_before {
                        &issue.call.caller
                    } else {
                        &issue.function
                    };
                let side = if issue.primary_before {
                    "before"
                } else {
                    "after"
                };
                let id = format!("bound-function:{}:{side}:{symbol}", report.change);
                if present.contains(&id) {
                    graph.edges.push(GraphEdge {
                        from: finding.id.clone(),
                        relation: "concerns_callable".into(),
                        to: id,
                    });
                }
            }
        }
    }
    graph.edges.sort();
    graph.edges.dedup();
}
