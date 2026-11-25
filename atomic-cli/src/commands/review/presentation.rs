//! File/region navigation shared by terminal and HTML. No analysis or new findings.

use atomic_canonical::review_finding::SourceSide;
use atomic_canonical::review_graph::{GraphNode, ReviewGraph};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct Region {
    pub id: String,
    pub change: String,
    pub path: String,
    pub side: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub subjects: BTreeSet<String>,
    pub findings: BTreeSet<u32>,
    pub metrics: BTreeSet<String>,
    pub context_metrics: BTreeSet<String>,
    pub impact: Vec<Value>,
}

type Key = (String, String, String, Option<u32>, Option<u32>);

fn at<'a>(
    regions: &'a mut BTreeMap<Key, Region>,
    change: &str,
    path: &str,
    side: &str,
    start: Option<u32>,
    end: Option<u32>,
) -> &'a mut Region {
    regions
        .entry((change.into(), path.into(), side.into(), start, end))
        .or_insert_with(|| Region {
            change: change.into(),
            path: path.into(),
            side: side.into(),
            start_line: start,
            end_line: end,
            ..Default::default()
        })
}

pub(super) fn metric_location(node: &GraphNode, side: &str) -> (Option<u32>, Option<u32>) {
    let location = &node.properties[format!("{side}_location")];
    (
        location["start_line"]
            .as_u64()
            .and_then(|n| n.try_into().ok()),
        location["end_line"]
            .as_u64()
            .and_then(|n| n.try_into().ok()),
    )
}

pub(super) fn regions(graph: &ReviewGraph) -> Vec<Region> {
    let mut regions = BTreeMap::new();
    let metrics: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| node.kind == "metric")
        .collect();
    for node in &metrics {
        let Some(path) = node.properties["path"].as_str() else {
            continue;
        };
        let change = node.properties["change"].as_str().unwrap_or("");
        let side = if metric_location(node, "after").0.is_some()
            || metric_location(node, "before").0.is_none()
        {
            "after"
        } else {
            "before"
        };
        let (start, end) = metric_location(node, side);
        let region = at(&mut regions, change, path, side, start, end);
        region.metrics.insert(node.id.clone());
        if let Some(subject) = node.properties["observation"]["subject"].as_str() {
            region.subjects.insert(subject_label(subject).into());
        }
    }
    for finding in &graph.findings {
        for location in &finding.finding.locations {
            let side = match location.side {
                SourceSide::Before => "before",
                SourceSide::After => "after",
            };
            at(
                &mut regions,
                &finding.finding.change,
                &location.path,
                side,
                Some(location.start_line),
                Some(location.end_line),
            )
            .findings
            .insert(finding.number);
        }
    }
    if let Some(risk) = &graph.risk {
        for report in &risk.impact {
            for function in &report.changed_functions {
                for (side, location) in [
                    ("before", function.before.as_ref()),
                    ("after", function.after.as_ref()),
                ] {
                    if let Some(location) = location {
                        let region = at(
                            &mut regions,
                            &report.change,
                            &location.path,
                            side,
                            Some(location.start_line),
                            Some(location.end_line),
                        );
                        region.subjects.insert(subject_label(&function.id).into());
                        region
                            .impact
                            .push(json!({"kind":"changed_function", "function":function}));
                    }
                }
            }
            for (side, kind, calls) in [
                ("before", "caller_before", &report.callers_before),
                ("after", "caller_after", &report.callers_after),
                ("after", "outgoing_call", &report.callees_after),
            ] {
                for call in calls {
                    at(
                        &mut regions,
                        &report.change,
                        &call.location.path,
                        side,
                        Some(call.location.start_line),
                        Some(call.location.end_line),
                    )
                    .impact
                    .push(json!({"kind":kind,"call":call}));
                }
            }
        }
    }
    // A branch-level finding can use the enclosing function's measurements as
    // context, but module totals are not repeated as if they measured the branch.
    for region in regions
        .values_mut()
        .filter(|region| !region.findings.is_empty() && region.metrics.is_empty())
    {
        let Some((start, end)) = region.start_line.zip(region.end_line) else {
            continue;
        };
        let enclosing: Vec<_> = metrics
            .iter()
            .filter_map(|node| {
                if node.properties["scope"] != "function"
                    || node.properties["path"] != region.path
                    || node.properties["change"] != region.change
                {
                    return None;
                }
                let (lo, hi) = metric_location(node, &region.side);
                lo.zip(hi)
                    .filter(|(lo, hi)| *lo <= start && *hi >= end)
                    .map(|(lo, hi)| (hi - lo, *node))
            })
            .collect();
        if let Some(minimum) = enclosing.iter().map(|(width, _)| *width).min() {
            for (_, node) in enclosing.into_iter().filter(|(width, _)| *width == minimum) {
                region.context_metrics.insert(node.id.clone());
                if let Some(subject) = node.properties["observation"]["subject"].as_str() {
                    region.subjects.insert(subject_label(subject).into());
                }
            }
        }
    }
    regions
        .into_values()
        .enumerate()
        .map(|(index, mut region)| {
            region.id = format!("region-{}", index + 1);
            region
        })
        .collect()
}

pub(super) fn subject_label(subject: &str) -> &str {
    let Some((_, local)) = subject.split_once('#') else {
        return subject;
    };
    let local = local
        .strip_prefix("function:")
        .or_else(|| local.strip_prefix("class:"))
        .unwrap_or(local);
    local
        .rsplit_once(':')
        .filter(|(_, ordinal)| !ordinal.is_empty() && ordinal.chars().all(|c| c.is_ascii_digit()))
        .map(|(name, _)| name)
        .unwrap_or(local)
}

pub(super) fn metric_id(node: &GraphNode) -> String {
    serde_json::from_value::<atomic_audit::Metric>(node.properties["observation"]["metric"].clone())
        .map(|metric| metric.id().into())
        .unwrap_or_else(|_| "metric.unknown".into())
}

pub(super) fn metric_value(node: &GraphNode) -> String {
    let observation = &node.properties["observation"];
    let label = |side: &str| {
        let value = &observation[side];
        match value["status"].as_str() {
            Some("measured") => value["value"].to_string(),
            Some("not_applicable") => "N/A".into(),
            _ => "unknown".into(),
        }
    };
    let delta = node.properties["delta"]
        .as_i64()
        .map(|n| format!(" ({n:+})"))
        .unwrap_or_default();
    if node.properties["scope_kind"] == "view_snapshot" {
        return format!("{} (snapshot)", label("after"));
    }
    format!("{} → {}{delta}", label("before"), label("after"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_preserve_qualified_names_without_inventing_anonymous_identity() {
        assert_eq!(
            subject_label("src/lib.rs#function:api::work:1"),
            "api::work"
        );
        assert_eq!(
            subject_label("src/lib.rs#function:f::<anonymous@42>:1"),
            "f::<anonymous@42>"
        );
        assert_eq!(subject_label("src/lib.rs"), "src/lib.rs");
    }

    #[test]
    fn physical_regions_preserve_all_metrics_and_keep_changes_and_sides_separate() {
        let node = |id: &str, change: &str, side: &str, subject: &str| {
            let mut properties = json!({"change":change,"path":"src/lib.rs","scope":"function","observation":{"metric":"method_length","subject":subject,"before":{"status":"measured","value":1},"after":{"status":"measured","value":2}}});
            properties[format!("{side}_location")] = json!({"start_line":10,"end_line":20});
            GraphNode {
                id: id.into(),
                kind: "metric".into(),
                label: id.into(),
                properties,
            }
        };
        let graph = ReviewGraph {
            schema: "atomic.review-graph.v1".into(),
            scope: atomic_canonical::review_graph::ReviewScope {
                project: "project".into(),
                view: "dev".into(),
                view_state: "state".into(),
                selected_roots: vec!["a".into(), "b".into()],
                dependency_closure: vec![],
                source_membership_hash: "source".into(),
                intent_sources: BTreeMap::new(),
            },
            passes: vec![],
            findings: vec![],
            criteria: vec![],
            edges: vec![],
            diagnostics: vec![],
            risk: None,
            nodes: vec![
                node("a1", "a", "after", "src/lib.rs#function:first:1"),
                node("a2", "a", "after", "src/lib.rs#function:second:1"),
                node("a3", "a", "before", "src/lib.rs#function:first:1"),
                node("b1", "b", "after", "src/lib.rs#function:first:1"),
            ],
        };
        let result = regions(&graph);
        assert_eq!(result.len(), 3);
        assert_eq!(
            result
                .iter()
                .map(|region| region.metrics.len())
                .sum::<usize>(),
            4
        );
        assert!(result
            .iter()
            .any(|region| region.metrics.len() == 2 && region.subjects.len() == 2));
        assert_eq!(
            result
                .iter()
                .map(|region| &region.id)
                .collect::<BTreeSet<_>>()
                .len(),
            3
        );
    }
}
