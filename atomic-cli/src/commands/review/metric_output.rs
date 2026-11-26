//! Compact terminal projection; the graph/JSON retains every observation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use atomic_canonical::review_graph::GraphNode;

const MAX_REASONS: usize = 6;
const MAX_PATHS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comparison {
    Up,
    Down,
    Same,
    New,
    Removed,
    Unknown,
    NotApplicable,
}

fn value(side: &serde_json::Value) -> Option<u64> {
    (side["status"] == "measured")
        .then(|| side["value"].as_u64())
        .flatten()
}

fn absent(side: &serde_json::Value) -> bool {
    side["status"] == "not_applicable"
        && matches!(
            side["reason"].as_str(),
            Some("File is absent in this revision" | "Subject is not present in this revision")
        )
}

fn comparison(node: &GraphNode) -> Comparison {
    let observation = &node.properties["observation"];
    let before = &observation["before"];
    let after = &observation["after"];
    match (value(before), value(after)) {
        (Some(before), Some(after)) => match after.cmp(&before) {
            std::cmp::Ordering::Greater => Comparison::Up,
            std::cmp::Ordering::Less => Comparison::Down,
            std::cmp::Ordering::Equal => Comparison::Same,
        },
        (None, Some(_)) if absent(before) => Comparison::New,
        (Some(_), None) if absent(after) => Comparison::Removed,
        (None, None)
            if before["status"] == "not_applicable" && after["status"] == "not_applicable" =>
        {
            Comparison::NotApplicable
        }
        _ => Comparison::Unknown,
    }
}

fn delta(node: &GraphNode) -> Option<i128> {
    let observation = &node.properties["observation"];
    value(&observation["before"])
        .zip(value(&observation["after"]))
        .map(|(before, after)| i128::from(after) - i128::from(before))
}

fn render_observation(text: &mut String, stem: &str, node: &GraphNode, details: bool) {
    let side = if value(&node.properties["observation"]["after"]).is_some() {
        "after"
    } else {
        "before"
    };
    let location = &node.properties[format!("{side}_location")];
    let anchor = location["start_line"]
        .as_u64()
        .map(|line| {
            let end = location["end_line"].as_u64().unwrap_or(line);
            format!(" · {side} line {line}-{end}")
        })
        .unwrap_or_default();
    let snapshot = node.properties["scope_kind"] == "view_snapshot";
    let change = if node.properties["scope_kind"] == "view_comparison" {
        " · recorded view comparison".into()
    } else if snapshot {
        " · recorded view snapshot".into()
    } else {
        node.properties["change"]
            .as_str()
            .map(|hash| format!(" · change {}", hash.chars().take(12).collect::<String>()))
            .unwrap_or_default()
    };
    writeln!(text, "{stem}  {}{anchor}{change}", node.label).unwrap();
    if details {
        for side in ["before", "after"] {
            let sample = &node.properties["observation"][side];
            if sample["status"] == "not_applicable" {
                if let Some(reason) = sample["reason"].as_str() {
                    writeln!(text, "{stem}    {side} N/A: {reason}").unwrap();
                }
            }
        }
    }
}

pub(super) fn render_metrics(text: &mut String, stem: &str, metrics: &[&GraphNode], details: bool) {
    let mut families: BTreeMap<String, Vec<&GraphNode>> = atomic_audit::METRIC_CARDS
        .iter()
        .map(|card| (card.title.into(), Vec::new()))
        .collect();
    for node in metrics {
        let value = &node.properties["observation"]["metric"];
        let metric = serde_json::from_value::<atomic_audit::Metric>(value.clone()).ok();
        let title = atomic_audit::METRIC_CARDS
            .iter()
            .find(|card| Some(card.metric) == metric)
            .map(|card| card.title)
            .unwrap_or_else(|| value.as_str().unwrap_or("Unknown metric"));
        families.entry(title.into()).or_default().push(node);
    }
    let width = families.keys().map(String::len).max().unwrap().max(6);
    if metrics
        .iter()
        .any(|node| node.properties["scope_kind"] == "view_snapshot")
    {
        writeln!(
            text,
            "{stem}View snapshot metrics (current subjects; no change comparison):"
        )
        .unwrap();
        writeln!(
            text,
            "{stem}{:<width$}  {:>8} {:>5} {:>11}",
            "Metric", "Measured", "N/A", "Unavailable"
        )
        .unwrap();
        for (title, observations) in &families {
            let measured = observations
                .iter()
                .filter(|node| value(&node.properties["observation"]["after"]).is_some())
                .count();
            let absent = observations
                .iter()
                .filter(|node| {
                    node.properties["observation"]["after"]["status"] == "not_applicable"
                })
                .count();
            if observations.is_empty() {
                writeln!(text, "{stem}{title:<width$}  not measured").unwrap();
            } else {
                writeln!(
                    text,
                    "{stem}{title:<width$}  {measured:>8} {absent:>5} {:>11}",
                    observations.len() - measured - absent
                )
                .unwrap();
            }
            if details {
                for node in observations {
                    render_observation(text, stem, node, true);
                }
            }
        }
        return;
    }
    writeln!(text, "{stem}Metric changes (subject counts, not findings):").unwrap();
    writeln!(
        text,
        "{stem}{:<width$}  {:>4} {:>4} {:>5} {:>5} {:>7} {:>7} {:>5}",
        "Metric", "Up", "Down", "Same", "New", "Removed", "Unknown", "N/A"
    )
    .unwrap();
    writeln!(text, "{stem}{}  {}", "-".repeat(width), "-".repeat(43)).unwrap();
    for (title, observations) in &mut families {
        observations
            .sort_by(|left, right| left.label.cmp(&right.label).then(left.id.cmp(&right.id)));
        if observations.is_empty() {
            writeln!(text, "{stem}{title:<width$}  not measured").unwrap();
            continue;
        }
        let count = |kind| {
            observations
                .iter()
                .filter(|node| comparison(node) == kind)
                .count()
        };
        writeln!(
            text,
            "{stem}{title:<width$}  {:>4} {:>4} {:>5} {:>5} {:>7} {:>7} {:>5}",
            count(Comparison::Up),
            count(Comparison::Down),
            count(Comparison::Same),
            count(Comparison::New),
            count(Comparison::Removed),
            count(Comparison::Unknown),
            count(Comparison::NotApplicable)
        )
        .unwrap();
    }
    writeln!(text, "{stem}Up/Down/Same compare numeric values, not code changes. New/Removed require recorded subject absence; Unknown has no reliable comparison.").unwrap();
    if details {
        writeln!(
            text,
            "{stem}All metric observations (before → after; no truncation):"
        )
        .unwrap();
        for observations in families.values() {
            for node in observations {
                render_observation(text, stem, node, true);
            }
        }
    } else {
        let mut highlights = String::new();
        for (title, observations) in &families {
            let mut increases: Vec<_> = observations
                .iter()
                .copied()
                .filter(|node| comparison(node) == Comparison::Up)
                .collect();
            increases
                .sort_by(|left, right| delta(right).cmp(&delta(left)).then(left.id.cmp(&right.id)));
            let mut new: Vec<_> = observations
                .iter()
                .copied()
                .filter(|node| comparison(node) == Comparison::New)
                .collect();
            new.sort_by(|left, right| {
                value(&right.properties["observation"]["after"])
                    .cmp(&value(&left.properties["observation"]["after"]))
                    .then(left.id.cmp(&right.id))
            });
            if increases.is_empty() && new.is_empty() {
                continue;
            }
            writeln!(
                highlights,
                "{stem}{title} — {} increase(s), {} new value(s):",
                increases.len(),
                new.len()
            )
            .unwrap();
            for node in increases.iter().take(3).chain(new.iter().take(1)) {
                render_observation(&mut highlights, stem, node, false);
            }
        }
        if !highlights.is_empty() {
            writeln!(text, "{stem}Review signals — largest 3 increases and highest new value per family; not defect verdicts:").unwrap();
            text.push_str(&highlights);
        }
        writeln!(
            text,
            "{stem}All values, decreases and missing-value reasons: --details or --json."
        )
        .unwrap();
    }
}

/// Deduplicate both repeated metric families and the same reason on both sides.
/// Keep real failures visible, including missing baselines on measured subjects.
pub(super) fn render_coverage(text: &mut String, nodes: &[GraphNode]) {
    let mut gaps: BTreeMap<&str, (BTreeSet<&str>, usize)> = BTreeMap::new();
    for node in nodes.iter().filter(|node| node.kind == "metric") {
        let reasons: BTreeSet<_> = ["before", "after"]
            .iter()
            .filter_map(|side| {
                let value = &node.properties["observation"][side];
                (value["status"] == "unavailable").then(|| {
                    value["reason"]
                        .as_str()
                        .unwrap_or("Unspecified analysis failure")
                })
            })
            .collect();
        for reason in reasons {
            let (paths, observations) = gaps.entry(reason).or_default();
            paths.insert(node.properties["path"].as_str().unwrap_or("unknown path"));
            *observations += 1;
        }
    }
    if gaps.is_empty() {
        return;
    }
    writeln!(text, "Metric coverage gaps (full observations in --json):").unwrap();
    for (reason, (paths, observations)) in gaps.iter().take(MAX_REASONS) {
        let sample = paths
            .iter()
            .copied()
            .take(MAX_PATHS)
            .collect::<Vec<_>>()
            .join(", ");
        let rest = if paths.len() > MAX_PATHS {
            format!(", +{} more", paths.len() - MAX_PATHS)
        } else {
            String::new()
        };
        writeln!(
            text,
            "  • {reason} — {} path(s), {observations} observation(s): {sample}{rest}",
            paths.len()
        )
        .unwrap();
    }
    if gaps.len() > MAX_REASONS {
        writeln!(
            text,
            "  {} further unavailable reasons retained in --json",
            gaps.len() - MAX_REASONS
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn node(path: &str, before: Value, after: Value) -> GraphNode {
        GraphNode {
            id: path.into(),
            kind: "metric".into(),
            label: format!("measured {path}: {before} → {after}"),
            properties: json!({"path": path, "observation": {"metric": "method_length", "before": before, "after": after}}),
        }
    }

    #[test]
    fn repeated_missing_facts_collapse_but_zero_and_partial_values_remain() {
        let unsupported = json!({"status": "unavailable", "reason": "Unsupported language"});
        let na = json!({"status": "not_applicable", "reason": "No class"});
        let mut nodes = vec![];
        for path in ["README.md", "package.json", "test/cli.test.mjs"] {
            for _ in 0..10 {
                nodes.push(node(path, unsupported.clone(), unsupported.clone()));
            }
        }
        nodes.push(node("empty.ts", na.clone(), na.clone()));
        nodes.push(node(
            "zero.ts",
            na,
            json!({"status": "measured", "value": 0}),
        ));
        nodes.push(node(
            "partial.ts",
            json!({"status": "unavailable", "reason": "Baseline read failed"}),
            json!({"status": "measured", "value": 2}),
        ));
        let snapshot = serde_json::to_value(&nodes).unwrap();
        let mut text = String::new();
        render_metrics(&mut text, "", &nodes.iter().collect::<Vec<_>>(), false);
        render_coverage(&mut text, &nodes);
        assert_eq!(
            counts(&text, "Method/function length"),
            [0, 0, 0, 0, 0, 32, 1]
        );
        assert!(!text.contains("measured zero.ts") && !text.contains("measured partial.ts"));
        assert!(!text.contains("measured empty.ts") && !text.contains("measured README.md"));
        assert_eq!(text.matches("Unsupported language").count(), 1);
        assert!(text
            .contains("3 path(s), 30 observation(s): README.md, package.json, test/cli.test.mjs"));
        assert!(text.contains("Baseline read failed"));
        assert_eq!(
            snapshot,
            serde_json::to_value(nodes).unwrap(),
            "rendering cannot discard JSON evidence"
        );
    }

    #[test]
    fn value_reason_and_path_lists_are_bounded_and_deterministic() {
        let mut nodes = vec![];
        for i in 0..30 {
            nodes.push(node(
                &format!("src/{i:02}.ts"),
                json!({"status": "measured", "value": 1}),
                json!({"status": "measured", "value": 2}),
            ));
            nodes.push(node(
                &format!("metadata/{i:02}.json"),
                json!({"status": "unavailable", "reason": "Unsupported language"}),
                json!({"status": "not_applicable"}),
            ));
        }
        for i in 0..8 {
            nodes.push(node(
                "broken.ts",
                json!({"status": "unavailable", "reason": format!("Failure {i}")}),
                json!({"status": "unavailable", "reason": "Unsupported language"}),
            ));
        }
        let render = |nodes: &[GraphNode]| {
            let mut text = String::new();
            render_metrics(&mut text, "", &nodes.iter().collect::<Vec<_>>(), false);
            render_coverage(&mut text, nodes);
            text
        };
        let text = render(&nodes);
        assert_eq!(
            counts(&text, "Method/function length"),
            [30, 0, 0, 0, 0, 38, 0]
        );
        assert!(!text.contains("further measured observations"));
        assert!(text.contains("3 further unavailable reasons retained in --json"));
        nodes.reverse();
        assert_eq!(text, render(&nodes));
        // Exercise path truncation without the reason cap hiding this group.
        let mut coverage = String::new();
        render_coverage(
            &mut coverage,
            &nodes
                .into_iter()
                .filter(|node| {
                    node.properties["path"]
                        .as_str()
                        .unwrap()
                        .starts_with("metadata/")
                })
                .collect::<Vec<_>>(),
        );
        assert!(coverage.contains("+24 more"));
    }

    #[test]
    fn every_measured_family_is_shown_without_a_location_cap() {
        let mut nodes = Vec::new();
        for metric in ["cyclomatic_complexity", "method_length", "public_surface"] {
            for i in 0..30 {
                let mut observation = node(
                    &format!("src/{metric}/{i}.rs"),
                    json!({"status": "measured", "value": 0}),
                    json!({"status": "measured", "value": 2}),
                );
                observation.properties["observation"]["metric"] = json!(metric);
                nodes.push(observation);
            }
        }
        let snapshot = serde_json::to_value(&nodes).unwrap();
        let render = |nodes: &[GraphNode]| {
            let mut text = String::new();
            render_metrics(&mut text, "│   ", &nodes.iter().collect::<Vec<_>>(), false);
            text
        };
        let text = render(&nodes);
        for title in [
            "Function cyclomatic complexity",
            "Method/function length",
            "Declared public surface",
        ] {
            assert_eq!(counts(&text, title), [30, 0, 0, 0, 0, 0, 0]);
        }
        assert!(text.lines().all(|line| line.starts_with("│   ")));
        assert!(text.contains("src/") && !text.contains("further measured"));
        for card in atomic_audit::METRIC_CARDS {
            assert!(
                text.contains(card.title),
                "{} must not disappear",
                card.title
            );
        }
        assert_eq!(snapshot, serde_json::to_value(&nodes).unwrap());
        nodes.reverse();
        assert_eq!(text, render(&nodes));
    }

    #[test]
    fn no_measurements_keep_families_visible_without_fabricating_values() {
        let mut text = String::new();
        render_metrics(&mut text, "", &[], false);
        assert_eq!(text.matches("not measured").count(), 10);
        text.clear();
        let missing = node(
            "src/lib.rs",
            json!({"status":"unavailable"}),
            json!({"status":"not_applicable"}),
        );
        render_metrics(&mut text, "", &[&missing], false);
        assert_eq!(
            counts(&text, "Method/function length"),
            [0, 0, 0, 0, 0, 1, 0]
        );
        assert_eq!(text.matches("not measured").count(), 9);
    }

    fn counts(text: &str, title: &str) -> Vec<usize> {
        text.lines()
            .find_map(|line| {
                let line = line.trim_start_matches("│   ").trim_start();
                line.strip_prefix(title)
                    .filter(|suffix| suffix.starts_with("  "))
                    .map(|suffix| {
                        suffix
                            .split_whitespace()
                            .map(|count| count.parse().unwrap())
                            .collect()
                    })
            })
            .unwrap()
    }

    #[test]
    fn categories_require_comparable_values_or_explicit_absence() {
        let measured = |value| json!({"status":"measured", "value":value});
        let absent =
            json!({"status":"not_applicable", "reason":"Subject is not present in this revision"});
        let unavailable = json!({"status":"unavailable", "reason":"Ambiguous identity"});
        let inapplicable = json!({"status":"not_applicable", "reason":"Declaration has no executable source body"});
        let cases = [
            (measured(0), measured(3), Comparison::Up),
            (measured(3), measured(0), Comparison::Down),
            (measured(0), measured(0), Comparison::Same),
            (absent.clone(), measured(9), Comparison::New),
            (measured(9), absent, Comparison::Removed),
            (unavailable, measured(9), Comparison::Unknown),
            (inapplicable.clone(), measured(9), Comparison::Unknown),
            (
                inapplicable.clone(),
                inapplicable,
                Comparison::NotApplicable,
            ),
        ];
        let nodes: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(i, (before, after, expected))| {
                let node = node(&format!("src/{i}.rs"), before.clone(), after.clone());
                assert_eq!(comparison(&node), *expected);
                node
            })
            .collect();
        let mut text = String::new();
        render_metrics(&mut text, "", &nodes.iter().collect::<Vec<_>>(), false);
        assert_eq!(
            counts(&text, "Method/function length"),
            [1, 1, 1, 1, 1, 2, 1]
        );
    }

    #[test]
    fn details_include_every_observation_and_default_ranks_values_not_names() {
        let mut nodes: Vec<_> = (0..40)
            .map(|i| {
                node(
                    &format!("src/{i:02}.rs"),
                    json!({"status":"measured", "value":0}),
                    json!({"status":"measured", "value":i + 1}),
                )
            })
            .collect();
        nodes[39].properties["after_location"] = json!({"start_line":42,"end_line":43});
        let refs = nodes.iter().collect::<Vec<_>>();
        let mut summary = String::new();
        render_metrics(&mut summary, "", &refs, false);
        assert!(summary.contains("src/39.rs") && summary.contains("after line 42"));
        assert!(!summary.contains("src/00.rs"));
        assert_eq!(summary.matches("  measured src/").count(), 3);
        let snapshot = serde_json::to_value(&nodes).unwrap();
        let mut details = String::new();
        render_metrics(&mut details, "", &refs, true);
        assert_eq!(details.matches("  measured src/").count(), 40);
        assert!(details.contains("src/00.rs") && details.contains("src/39.rs"));
        assert_eq!(snapshot, serde_json::to_value(&nodes).unwrap());
    }
}
