//! Adapt existing semantic/metric observations, without another source scan.

use std::collections::{BTreeMap, BTreeSet};

use atomic_audit::{
    impact::ImpactReport,
    metrics::EXTRACTION_CONVENTION,
    risk::{component, test_support, RiskFacts},
};
use atomic_canonical::review_graph::GraphNode;

fn code(path: &str) -> bool {
    atomic_semantic::is_supported(path)
        || matches!(
            std::path::Path::new(path)
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("js" | "jsx" | "mjs" | "cjs")
        )
}

fn reference(evidence: &mut BTreeMap<String, BTreeSet<String>>, dimension: &str, value: String) {
    let values = evidence.entry(dimension.into()).or_default();
    values.insert(value);
    // Deterministic bounded references; full source observations remain in graph.
    if values.len() > 8 {
        values.pop_last();
    }
}

pub(super) fn collect(nodes: &[GraphNode], impact: &[ImpactReport]) -> RiskFacts {
    let mut facts = RiskFacts::default();
    let (mut functions, mut existing, mut signatures, mut tests) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    let (
        mut paths,
        mut non_callable,
        mut prod_callers,
        mut test_callers,
        mut existing_callers,
        mut caller_files,
        mut outside,
    ) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    let mut refs = BTreeMap::new();
    let mut recorded_paths = BTreeMap::<String, BTreeSet<String>>::new();
    for node in nodes.iter().filter(|node| node.kind == "metric") {
        if let (Some(change), Some(path)) = (
            node.properties["change"].as_str(),
            node.properties["path"].as_str(),
        ) {
            recorded_paths
                .entry(change.into())
                .or_default()
                .insert(path.into());
        }
    }
    for node in nodes.iter().filter(|node| node.kind == "finding") {
        if node.properties["comparison"].is_object()
            && node.properties["comparison"]["status"] != "new"
        {
            continue;
        }
        let finding = &node.properties["finding"];
        if finding["introduced_by_change"]["status"] != "no"
            && matches!(
                finding["requirement_or_rule"]["id"].as_str(),
                Some("structural.syntax" | "anti-slop.rust-placeholder" | "anti-slop.empty-catch")
            )
        {
            facts.structural_alerts += 1;
            facts.available = true;
            reference(
                &mut refs,
                "alerts",
                format!(
                    "{}:{}",
                    finding["change"].as_str().unwrap_or("recorded"),
                    finding["requirement_or_rule"]["id"].as_str().unwrap()
                ),
            );
        }
    }
    let mut unresolved = Some(0usize);
    let mut selection = Some(true);
    for report in impact {
        reference(
            &mut refs,
            "change",
            format!("{}:semantic-source-comparison", report.change),
        );
        facts.available |= report.inspected_files_before > 0 || report.inspected_files_after > 0;
        selection = match (selection, report.source_selection_complete) {
            (Some(a), Some(b)) => Some(a && b),
            _ => None,
        };
        unresolved = unresolved
            .zip(report.unresolved_references)
            .map(|(a, b)| a.saturating_add(b));
        let mut old_ids = BTreeSet::new();
        let mut selected_paths = recorded_paths
            .get(&report.change)
            .cloned()
            .unwrap_or_default();
        selected_paths.extend(report.non_callable_changes.iter().cloned());
        for function in &report.changed_functions {
            let Some(location) = function.after.as_ref().or(function.before.as_ref()) else {
                continue;
            };
            selected_paths.insert(location.path.clone());
            if test_support(&location.path, &function.id) {
                tests.insert(function.id.clone());
                continue;
            }
            functions.insert(function.id.clone());
            paths.insert(location.path.clone());
            if function.before.is_some() {
                existing.insert(function.id.clone());
                old_ids.insert(function.id.clone());
            }
            if function.signature_changed && function.before.is_some() {
                signatures.insert(function.id.clone());
            }
            reference(
                &mut refs,
                "change",
                format!(
                    "{}:{}:{}:{}-{}",
                    report.change,
                    if function.after.is_some() {
                        "after"
                    } else {
                        "before"
                    },
                    location.path,
                    location.start_line,
                    location.end_line
                ),
            );
        }
        for path in &report.non_callable_changes {
            if code(path) && !test_support(path, "") {
                non_callable.insert(path.clone());
                paths.insert(path.clone());
                reference(
                    &mut refs,
                    "change",
                    format!("{}:module:{path}", report.change),
                );
            }
        }
        for call in &report.callers_after {
            if !call.target_present || !call.visibility_compatible {
                continue;
            }
            if test_support(&call.location.path, &call.caller) {
                test_callers.insert(call.caller.clone());
            } else {
                prod_callers.insert(call.caller.clone());
            }
        }
        for call in report.callers_before.iter().chain(&report.callers_after) {
            if !call.visibility_compatible
                || test_support(&call.location.path, &call.caller)
                || !old_ids.contains(&call.callee)
            {
                continue;
            }
            existing_callers.insert(call.caller.clone());
            caller_files.insert(call.location.path.clone());
            if !selected_paths.contains(&call.location.path) {
                outside.insert(call.caller.clone());
            }
            reference(
                &mut refs,
                "calls",
                format!(
                    "{}:{}:{}-{}:{}",
                    report.change,
                    call.location.path,
                    call.location.start_line,
                    call.location.end_line,
                    call.caller
                ),
            );
        }
    }
    if impact.is_empty() {
        selection = None;
        unresolved = None;
    }
    let (
        mut public_seen,
        mut public_complete,
        mut public_growth,
        mut public_added,
        mut public_removed,
        mut dep_seen,
        mut dep_growth,
    ) = (false, true, 0u64, 0u64, 0u64, false, 0u64);
    let mut seen = BTreeSet::new();
    for node in nodes.iter().filter(|node| node.kind == "metric") {
        if !seen.insert(&node.id) {
            continue;
        }
        let Some(path) = node.properties["path"]
            .as_str()
            .filter(|path| code(path) && !test_support(path, ""))
        else {
            continue;
        };
        let observation = &node.properties["observation"];
        let Some(metric) = observation["metric"]
            .as_str()
            .filter(|metric| matches!(*metric, "public_surface" | "dependency_fan_out"))
        else {
            continue;
        };
        let before = &observation["before"];
        let after = &observation["after"];
        let measured = |value: &serde_json::Value| {
            (value["status"] == "measured")
                .then(|| value["value"].as_u64())
                .flatten()
        };
        let supported = node.properties["convention"] == EXTRACTION_CONVENTION;
        let comparable = supported
            && (measured(before).is_some() || before["status"] == "not_applicable")
            && (measured(after).is_some() || after["status"] == "not_applicable");
        if metric == "public_surface" {
            public_seen = true;
            public_complete &= comparable;
        } else {
            dep_seen = true;
        }
        if !comparable {
            continue;
        }
        facts.available = true;
        let delta = measured(before)
            .zip(measured(after))
            .map(|(before, after)| after.saturating_sub(before));
        let added = if before["status"] == "not_applicable" {
            measured(after)
        } else {
            None
        };
        let removed = measured(before)
            .map(|before| {
                if after["status"] == "not_applicable" {
                    before
                } else {
                    before.saturating_sub(measured(after).unwrap_or(before))
                }
            })
            .unwrap_or(0);
        if metric == "public_surface" {
            public_growth = public_growth.saturating_add(delta.unwrap_or(0));
            public_added = public_added.saturating_add(added.unwrap_or(0));
            public_removed = public_removed.saturating_add(removed);
        } else {
            dep_growth = dep_growth.saturating_add(delta.unwrap_or(0));
        }
        if delta.is_some_and(|value| value > 0)
            || added.is_some_and(|value| value > 0)
            || (metric == "public_surface" && removed > 0)
        {
            paths.insert(path.into());
            reference(
                &mut refs,
                if metric == "public_surface" {
                    "surface"
                } else {
                    "dependency"
                },
                format!(
                    "{}:{metric}:{path}",
                    node.properties["change"].as_str().unwrap_or("recorded")
                ),
            );
        }
    }
    facts.production_functions = functions.len();
    facts.existing_production_functions = existing.len();
    facts.signature_changes = signatures.len();
    facts.test_support_functions = tests.len();
    facts.components = paths
        .iter()
        .map(|path| component(path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    facts.production_paths = paths.into_iter().collect();
    facts.non_callable_paths = non_callable.into_iter().collect();
    facts.observed_production_callers = prod_callers.len();
    facts.observed_test_support_callers = test_callers.len();
    facts.existing_production_callers = existing_callers.len();
    facts.existing_caller_files = caller_files.len();
    facts.outside_change_callers = outside.len();
    facts.public_growth = public_seen.then_some(public_growth);
    facts.added_file_public_units = public_seen.then_some(public_added);
    facts.public_removed = public_seen.then_some(public_removed);
    facts.dependency_growth = dep_seen.then_some(dep_growth);
    facts.source_selection_complete = selection;
    facts.unresolved_references = unresolved;
    facts.surface_measurements_complete = (public_seen && public_complete)
        || (facts.production_functions == 0
            && facts.test_support_functions > 0
            && facts.production_paths.is_empty());
    if selection != Some(true) {
        facts
            .unknowns
            .push("Source-selection coverage is incomplete or unavailable".into());
    }
    if unresolved != Some(0) {
        facts
            .unknowns
            .push("Unresolved reference expressions are excluded from observed callers".into());
    }
    if !facts.surface_measurements_complete {
        facts
            .unknowns
            .push("Declared public-surface measurements are incomplete or unavailable".into());
    }
    facts.references = refs
        .into_iter()
        .map(|(key, refs)| (key, refs.into_iter().collect()))
        .collect();
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_audit::impact::{ChangedFunction, DirectCall, SourceLocation};
    use serde_json::{json, Value};

    #[test]
    fn existing_or_resolved_view_observations_do_not_become_new_structural_risk_alerts() {
        for status in ["existing", "resolved", "changed", "unknown"] {
            let node = GraphNode {
                id: "finding".into(),
                kind: "finding".into(),
                label: "placeholder".into(),
                properties: json!({"comparison":{"status":status},"finding":{"change":"urn:comparison","introduced_by_change":{"status":"unknown"},"requirement_or_rule":{"id":"anti-slop.rust-placeholder"}}}),
            };
            assert_eq!(collect(&[node], &[]).structural_alerts, 0);
        }
    }

    fn location(path: &str) -> SourceLocation {
        SourceLocation {
            path: path.into(),
            start_line: 1,
            end_line: 1,
        }
    }
    fn metric(id: &str, path: &str, before: Value, after: Value) -> GraphNode {
        GraphNode {
            id: id.into(),
            kind: "metric".into(),
            label: "".into(),
            properties: json!({"change":"hash","path":path,"convention":EXTRACTION_CONVENTION,"observation":{"metric":"public_surface","before":before,"after":after}}),
        }
    }
    fn call(caller: &str, path: &str) -> DirectCall {
        DirectCall {
            caller: caller.into(),
            callee: "src/lib.rs#work".into(),
            location: location(path),
            declaration: location("src/lib.rs"),
            binding: "explicit binding".into(),
            arguments: Some(0),
            target_present: true,
            visibility_compatible: true,
            configuration_dependent: false,
        }
    }

    #[test]
    fn facts_deduplicate_consumers_separate_tests_and_use_recorded_path_scope() {
        let production = ChangedFunction {
            id: "src/lib.rs#work".into(),
            before: Some(location("src/lib.rs")),
            after: Some(location("src/lib.rs")),
            change: "modified".into(),
            signature_changed: false,
        };
        let test = ChangedFunction {
            id: "src/lib.rs#tests::helper".into(),
            before: None,
            after: Some(location("src/lib.rs")),
            change: "added".into(),
            signature_changed: false,
        };
        let report = ImpactReport {
            change: "hash".into(),
            inspected_files_after: 2,
            changed_functions: vec![production, test],
            callers_after: vec![
                call("consumer.rs#consumer", "consumer.rs"),
                call("consumer.rs#consumer", "consumer.rs"),
                call("src/lib.rs#tests::run", "src/lib.rs"),
            ],
            source_selection_complete: Some(true),
            unresolved_references: Some(0),
            ..Default::default()
        };
        let measured = |value| json!({"status":"measured","value":value});
        let nodes = vec![
            metric("lib", "src/lib.rs", measured(1), measured(2)),
            metric("consumer", "consumer.rs", measured(0), measured(0)),
            metric("tests", "src/tests.rs", measured(0), measured(1000)),
        ];
        let facts = collect(&nodes, &[report]);
        assert_eq!(facts.production_functions, 1);
        assert_eq!(facts.test_support_functions, 1);
        assert_eq!(facts.observed_production_callers, 1);
        assert_eq!(facts.observed_test_support_callers, 1);
        assert_eq!(facts.existing_production_callers, 1);
        assert_eq!(
            facts.outside_change_callers, 0,
            "consumer.rs is selected even though its functions did not change"
        );
        assert_eq!(facts.public_growth, Some(1));
    }

    #[test]
    fn unavailable_baselines_cannot_invent_public_growth_or_complete_measurement() {
        let node = metric(
            "unknown",
            "src/lib.rs",
            json!({"status":"unavailable","reason":"baseline unavailable"}),
            json!({"status":"measured","value":500}),
        );
        let facts = collect(&[node], &[]);
        assert_eq!(
            facts.public_growth,
            Some(0),
            "zero is the measured lower bound, not a replacement baseline"
        );
        assert!(!facts.surface_measurements_complete);
        assert!(!facts.unknowns.is_empty());
        let node = metric(
            "removed",
            "src/lib.rs",
            json!({"status":"measured","value":5}),
            json!({"status":"measured","value":3}),
        );
        assert_eq!(collect(&[node], &[]).public_removed, Some(2));
    }
}
