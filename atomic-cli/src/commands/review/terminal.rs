//! Location-first terminal review. Card/evidence expansion is opt-in.

use super::presentation;
use atomic_canonical::review_graph::{PassStatus, ReviewGraph, ReviewPass};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

pub(super) fn render(graph: &ReviewGraph, details: bool) -> String {
    let snapshot = graph.nodes.iter().find(|node| node.kind == "view_snapshot");
    let comparison = graph
        .nodes
        .iter()
        .find(|node| node.kind == "view_comparison");
    let mut text = format!(
        "Review — {} @ {}\nLocal semantic audits and metrics · no AI execution\nNot independent signed approval · {} finding(s)\n{}\n",
        graph.scope.view,
        graph.scope.view_state,
        graph.findings.len(),
        if let Some(comparison)=comparison {format!("View comparison: BEFORE {} -> AFTER {}",comparison.properties["from"]["view"].as_str().unwrap_or("?"),comparison.properties["to"]["view"].as_str().unwrap_or("?"))} else if snapshot.is_some(){format!("Selected view snapshot: {}",graph.scope.view)}else{format!("Selected changes: {}",graph.scope.selected_roots.join(", "))}
    );
    if let Some(snapshot) = snapshot {
        writeln!(
            text,
            "Entire recorded view snapshot · {} live paths · no before/change baseline",
            snapshot.properties["paths"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0)
        )
        .unwrap();
    }
    if let Some(comparison) = comparison {
        for side in ["from", "to"] {
            writeln!(
                text,
                "  {side}: {} · state {} · filter {}",
                comparison.properties[side]["view"].as_str().unwrap_or("?"),
                comparison.properties[side]["state"].as_str().unwrap_or("?"),
                comparison.properties[side]["membership_hash"]
                    .as_str()
                    .unwrap_or("?")
            )
            .unwrap();
        }
        let mut counts = BTreeMap::<&str, usize>::new();
        for node in graph
            .nodes
            .iter()
            .filter(|node| node.kind == "compared_file")
        {
            *counts
                .entry(node.properties["status"].as_str().unwrap_or("unavailable"))
                .or_default() += 1;
            if details {
                writeln!(
                    text,
                    "  {}: {} -> {}",
                    node.properties["status"].as_str().unwrap_or("unavailable"),
                    node.properties["before_path"]
                        .as_str()
                        .unwrap_or("(absent)"),
                    node.properties["after_path"].as_str().unwrap_or("(absent)")
                )
                .unwrap();
            }
        }
        writeln!(
            text,
            "Recorded file states: {}",
            counts
                .into_iter()
                .map(|(status, count)| format!("{count} {status}"))
                .collect::<Vec<_>>()
                .join(" · ")
        )
        .unwrap();
    }
    let regions = presentation::regions(graph);
    for node in graph
        .nodes
        .iter()
        .filter(|node| node.kind == "analysis_exclusion")
    {
        writeln!(
            text,
            "Excluded from code analysis: {} .vault metadata path(s), retained as evidence",
            node.properties["paths"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0)
        )
        .unwrap();
    }
    let nodes: BTreeMap<_, _> = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    for pass in &graph.passes {
        writeln!(text, "\n{} [{:?}]", pass.pass.label(), pass.status).unwrap();
        if pass.status == PassStatus::NotApplicable {
            writeln!(text, "{}", pass.detail).unwrap();
            continue;
        }
        if pass.pass == ReviewPass::BlastRadiusRisk {
            if let Some(risk) = &graph.risk {
                super::risk::render(&mut text, "", risk);
            }
        }
        let findings: BTreeMap<_, _> = graph
            .findings
            .iter()
            .filter(|finding| finding.passes.contains(&pass.pass))
            .map(|finding| (finding.number, finding))
            .collect();
        let mut previous = None;
        let mut printed = BTreeSet::new();
        for region in &regions {
            let relevant: Vec<_> = region
                .findings
                .iter()
                .filter_map(|number| findings.get(number))
                .collect();
            if relevant.is_empty() {
                continue;
            }
            let file = (&region.change, &region.path);
            if previous != Some(file) {
                writeln!(
                    text,
                    "\n{}  [{} {}]",
                    region.path,
                    if comparison.is_some() {
                        "view comparison"
                    } else if snapshot.is_some() {
                        "view snapshot"
                    } else {
                        "change"
                    },
                    if comparison.is_some() || snapshot.is_some() {
                        graph.scope.view.clone()
                    } else {
                        region.change.chars().take(12).collect::<String>()
                    }
                )
                .unwrap();
                previous = Some(file);
            }
            let range = region
                .start_line
                .zip(region.end_line)
                .map(|(lo, hi)| format!("{lo}–{hi}"))
                .unwrap_or_else(|| "unavailable range".into());
            let subjects = region
                .subjects
                .iter()
                .filter(|subject| **subject != region.path)
                .collect::<Vec<_>>();
            let name = match subjects.as_slice() {
                [subject] => (*subject).clone(),
                [] => "source region".into(),
                many => format!("{} source subjects", many.len()),
            };
            writeln!(text, "  {range} ({})  {name}", region.side).unwrap();
            for finding in relevant {
                printed.insert(finding.number);
                if let Some(state) = &finding.comparison {
                    writeln!(
                        text,
                        "    Comparison observation: {} — {}",
                        state.status, state.rationale
                    )
                    .unwrap();
                }
                writeln!(
                    text,
                    "    F{} · {:?} / {:?}: {}",
                    finding.number,
                    finding.finding.severity,
                    finding.finding.confidence,
                    finding.finding.summary
                )
                .unwrap();
                writeln!(
                    text,
                    "    Audit/rule: {} · introduced: {:?}",
                    finding.finding.requirement_or_rule.id,
                    finding.finding.introduced_by_change.status
                )
                .unwrap();
                if details {
                    writeln!(
                        text,
                        "    Attribution: {}",
                        finding.finding.introduced_by_change.rationale
                    )
                    .unwrap();
                    for evidence in &finding.finding.evidence {
                        writeln!(
                            text,
                            "    Evidence {}: {}",
                            evidence.reference, evidence.observation
                        )
                        .unwrap();
                    }
                    writeln!(
                        text,
                        "    Correction: {}",
                        finding.finding.proposed_correction
                    )
                    .unwrap();
                    for criterion in &finding.finding.acceptance_criteria {
                        writeln!(
                            text,
                            "    AC {} → {:?}: {}",
                            criterion.criterion, criterion.assessment, criterion.rationale
                        )
                        .unwrap();
                    }
                    if finding.finding.acceptance_criteria.is_empty() {
                        writeln!(text,"    AC mapping: unresolved (engineering rule only; no unmet criterion inferred)").unwrap();
                    }
                }
            }
            let metrics = if region.metrics.is_empty() {
                &region.context_metrics
            } else {
                &region.metrics
            };
            let has_functions = metrics.iter().any(|id| {
                nodes
                    .get(id.as_str())
                    .is_some_and(|node| node.properties["scope"] == "function")
            });
            for id in metrics {
                if let Some(node) = nodes.get(id.as_str()) {
                    if has_functions && node.properties["scope"] != "function" && !details {
                        continue;
                    }
                    if presentation::metric_id(node) == "metric.parameter-count" && !details {
                        continue;
                    }
                    let subject = if subjects.len() > 1 {
                        node.properties["observation"]["subject"]
                            .as_str()
                            .map(|name| format!(" [{}]", presentation::subject_label(name)))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    writeln!(
                        text,
                        "    {}{subject}: {}{}",
                        presentation::metric_id(node),
                        presentation::metric_value(node),
                        if region.context_metrics.contains(id) {
                            " (enclosing function)"
                        } else {
                            ""
                        }
                    )
                    .unwrap();
                }
            }
        }
        // Findings without coordinates still remain visible; never drop an
        // imported judgment just because file navigation cannot locate it.
        for finding in findings
            .values()
            .filter(|finding| !printed.contains(&finding.number))
        {
            writeln!(
                text,
                "F{} · {:?}: {} [{}]",
                finding.number,
                finding.finding.severity,
                finding.finding.summary,
                finding.finding.requirement_or_rule.id
            )
            .unwrap();
        }
        if pass.pass == ReviewPass::IntentConformance {
            if let Some(view) = comparison.or(snapshot) {
                let context = &view.properties["conformance"];
                if let Some(intents) = context["intents"].as_array() {
                    writeln!(
                        text,
                        "View-scoped intents: {} ({}); assignment is context, not acceptance proof",
                        intents.len(),
                        context["scope"]["view"].as_str().unwrap_or("?")
                    )
                    .unwrap();
                    for intent in intents {
                        writeln!(
                            text,
                            "  {} · {} [{}; {}]",
                            intent["human_key"].as_str().unwrap_or("?"),
                            intent["title"].as_str().unwrap_or("?"),
                            intent["status"].as_str().unwrap_or("unknown"),
                            intent["source_kind"].as_str().unwrap_or("unavailable")
                        )
                        .unwrap();
                    }
                }
                if let Some(changes) = context["changes"]
                    .as_array()
                    .filter(|_| comparison.is_some())
                {
                    writeln!(
                        text,
                        "Recorded membership delta: {} change(s) added to To, {} only in From",
                        changes
                            .iter()
                            .filter(|change| change["direction"] == "added_to_after")
                            .count(),
                        changes
                            .iter()
                            .filter(|change| change["direction"] == "removed_from_after")
                            .count()
                    )
                    .unwrap();
                }
                if let Some(criteria) = context["criteria"].as_array() {
                    for criterion in criteria {
                        writeln!(
                            text,
                            "  {}: before declared={} -> after declared={}{}",
                            criterion["id"].as_str().unwrap_or("?"),
                            criterion["before"]["status"]
                                .as_str()
                                .unwrap_or("unavailable"),
                            criterion["after"]["status"]
                                .as_str()
                                .unwrap_or("unavailable"),
                            if criterion["current_context"].is_object() {
                                "; current vault fallback is context only"
                            } else {
                                "; pinned recorded definitions"
                            }
                        )
                        .unwrap();
                    }
                }
            }
            if graph.criteria.is_empty() {
                writeln!(text, "{}", pass.detail).unwrap();
            }
            for criterion in &graph.criteria {
                writeln!(
                    text,
                    "AC {} · review={:?} · declared={} · findings={}\n  {}",
                    criterion.id,
                    criterion.review_assessment,
                    criterion.declared_status,
                    criterion
                        .findings
                        .iter()
                        .map(|n| format!("F{n}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    criterion.text
                )
                .unwrap();
            }
        } else if pass.pass == ReviewPass::AntiSlop {
            if findings.is_empty() {
                writeln!(
                    text,
                    "No numbered audit findings in the supported checks; not a clean-code verdict."
                )
                .unwrap();
            }
            let count = |status: &str| {
                graph
                    .nodes
                    .iter()
                    .filter(|node| node.kind == "check" && node.properties["status"] == status)
                    .count()
            };
            writeln!(
                text,
                "\nChecks: {} completed · {} skipped · {} errors",
                count("completed"),
                count("skipped"),
                count("error")
            )
            .unwrap();
            if details {
                render_details(&mut text, graph);
            } else if snapshot.is_some() || comparison.is_some() {
                let metrics = graph
                    .nodes
                    .iter()
                    .filter(|node| node.kind == "metric")
                    .collect::<Vec<_>>();
                super::metric_output::render_metrics(&mut text, "", &metrics, false);
            }
        }
    }
    for diagnostic in &graph.diagnostics {
        writeln!(text, "Note: {diagnostic}").unwrap();
    }
    writeln!(text,"\nBrowse files, recorded source and audit/metric cards: --html\nFull measurements and evidence: --details or --json").unwrap();
    text
}

fn render_details(text: &mut String, graph: &ReviewGraph) {
    for card in atomic_audit::AUDIT_CARDS {
        let count = |status: &str| {
            graph
                .nodes
                .iter()
                .filter(|node| {
                    node.kind == "check"
                        && node.properties["rule"] == card.id
                        && node.properties["status"] == status
                })
                .count()
        };
        let findings = graph
            .findings
            .iter()
            .filter(|finding| finding.finding.requirement_or_rule.id == card.id)
            .count();
        writeln!(text,"Audit {}: {} finding(s) · {} completed · {} skipped · {} errors\n  Card {}: {}\n  Evidence: {}\n  Exceptions: {}",card.title,findings,count("completed"),count("skipped"),count("error"),card.id,card.question,card.evidence,card.exceptions).unwrap();
    }
    let metrics = graph
        .nodes
        .iter()
        .filter(|node| node.kind == "metric")
        .collect::<Vec<_>>();
    super::metric_output::render_metrics(text, "", &metrics, true);
    super::metric_output::render_coverage(text, &graph.nodes);
}
