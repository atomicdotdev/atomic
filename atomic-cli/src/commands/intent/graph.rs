//! `atomic intent graph <ID>` — semantic lineage, not a search neighborhood.
//!
//! Terminal output is a grouped tree with one line per selected node. Groups
//! organize evidence; they do not assert parent/child relationships in the KG.
//! Use `--details` for complete text and canonical relationships, `--json` for
//! machine-readable facts, or `--html` for the interactive browser graph.

use crate::{
    commands::Command,
    error::{CliError, CliResult},
};
use atomic_core::kg::ontology::{compact, Class};
use atomic_repository::intent_graph::{IntentGraph as Graph, DEFAULT_MAX_NODES};
use clap::Parser;

#[derive(Debug, Parser)]
pub struct IntentGraph {
    /// Human intent key, UID, or canonical intent IRI.
    pub id: String,
    /// Emit canonical nodes, predicates, roles, snapshot identity, and diagnostics.
    #[arg(long, conflicts_with_all = ["dot", "html"])]
    pub json: bool,
    /// Emit Graphviz DOT (pipe to `dot -Tsvg` to render).
    #[arg(long, conflicts_with = "html")]
    pub dot: bool,
    /// Save an interactive HTML graph and open it in the default browser.
    #[arg(long)]
    pub html: bool,
    /// Expand the tree with full labels, canonical IDs, summaries and relationships.
    #[arg(long, conflicts_with_all = ["json", "dot", "html"])]
    pub details: bool,
    /// Do not open a browser; emit HTML to stdout or the --output file.
    #[arg(long, requires = "html")]
    pub no_open: bool,
    /// Maximum selected nodes; limits are explicit, never silent truncation.
    #[arg(long, default_value_t = DEFAULT_MAX_NODES as u32, value_parser = clap::value_parser!(u32).range(1..=10000))]
    pub max_nodes: u32,
    /// Write output to this file (HTML also opens it unless --no-open is set).
    #[arg(short, long)]
    pub output: Option<std::path::PathBuf>,
}

impl Command for IntentGraph {
    fn run(&self) -> CliResult<()> {
        let graph = crate::commands::rpc::intent_graph(&self.id, self.max_nodes as usize)
            .map_err(|error| {
                if error.to_string().contains("KgNeedsRebuild") {
                    CliError::InvalidArgument {
                        message: "workflow graph needs an explicit upgrade; run atomic query reindex --full, then retry intent graph".into(),
                    }
                } else { error }
            })?;
        let output = if self.json {
            serde_json::to_string_pretty(&graph).map_err(|e| CliError::Internal(e.into()))? + "\n"
        } else if self.dot {
            let mut nodes = graph.nodes.clone();
            annotate(&graph, &mut nodes);
            let mut dot = crate::commands::query::emit_dot(&nodes, &graph.edges);
            let notes = graph
                .diagnostics
                .iter()
                .map(|s| format!("// {}\n", s.replace(['\n', '\r'], " ")))
                .collect::<String>();
            dot.insert_str(0, &notes);
            dot
        } else if self.html {
            let mut nodes = graph.nodes.clone();
            annotate(&graph, &mut nodes);
            let title = format!(
                "Intent {} — {} implementing, {} intent-artifact recording, {} dependency-only changes{}",
                self.id,
                graph.implementing_changes.len(),
                graph.recording_changes.len(),
                graph.dependency_changes.len(),
                if graph.truncated { " [TRUNCATED]" } else { "" }
            );
            let html = crate::commands::query::emit_html(&title, &nodes, &graph.edges);
            let notes = format!(
                "<details style=\"position:absolute;top:8px;left:16px;z-index:15;max-width:min(650px, calc(100% - 32px));background:#1a1a2e;padding:10px\"><summary>Scope and evidence notes</summary><pre style=\"white-space:pre-wrap\">{}</pre></details>",
                escape_html(&format!("Snapshot: {}\nScope: {}\n{}", graph.snapshot_identity, graph.scope, graph.diagnostics.join("\n")))
            );
            html.replacen(
                "<canvas id=\"graph\">",
                &format!("{notes}<canvas id=\"graph\">"),
                1,
            )
        } else if self.details {
            render_details(&graph)
        } else {
            render_text(&graph)
        };
        // Text already includes the notes; exports keep diagnostics off stdout.
        if self.json || self.dot || self.html {
            for diagnostic in &graph.diagnostics {
                eprintln!("Note: {diagnostic}");
            }
        }
        if self.html && !self.no_open {
            write_and_open_html(&output, self.output.as_deref(), |url| open::that(url))?;
        } else if let Some(path) = &self.output {
            std::fs::write(path, output)?;
        } else {
            print!("{output}");
        }
        Ok(())
    }
}

fn write_and_open_html(
    html: &str,
    output: Option<&std::path::Path>,
    open_browser: impl FnOnce(&str) -> std::io::Result<()>,
) -> CliResult<std::path::PathBuf> {
    use std::io::Write;

    let path = match output {
        Some(path) => {
            std::fs::write(path, html)?;
            path.to_path_buf()
        }
        None => {
            // Keep the file after the process exits so asynchronous browsers
            // can load it. Unique names avoid replacing another open graph.
            let path = std::env::temp_dir()
                .join(format!("atomic-intent-graph-{}.html", uuid::Uuid::new_v4()));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(html.as_bytes())?;
            path
        }
    };
    let path = std::fs::canonicalize(path)?;
    let url = url::Url::from_file_path(&path).map_err(|_| CliError::InvalidArgument {
        message: format!("cannot convert HTML path to a file URL: {}", path.display()),
    })?;
    println!("Wrote intent graph to {}", path.display());
    if let Err(error) = open_browser(url.as_str()) {
        eprintln!("Could not open a browser: {error}. Open {url} manually.");
    }
    Ok(path)
}

fn annotate(graph: &Graph, nodes: &mut [atomic_core::KgNode]) {
    for node in nodes {
        let role = if node.id == graph.root {
            "root intent"
        } else if graph.declared_changes.contains(&node.id)
            && !graph.captured_changes.contains(&node.id)
        {
            "declared implementation; original turn provenance not established"
        } else if graph.implementing_changes.contains(&node.id) {
            "implementing change"
        } else if graph.recording_changes.contains(&node.id) {
            "records intent artifact; implementation attribution not asserted"
        } else if graph.intent_artifact.as_ref() == Some(&node.id) {
            "selected intent's recorded artifact (matched by stored vault_path)"
        } else if graph.dependency_changes.contains(&node.id) {
            "dependency-only change"
        } else {
            "context"
        };
        let state = node.metadata.as_ref().map(|m| {
            format!(
                "object={}, projection={}",
                m.get("object_availability")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
                m.get("projection_completeness")
                    .and_then(|v| v.as_str())
                    .unwrap_or("reference-only")
            )
        });
        let evidence = if node.has_type(atomic_core::kg::ontology::Class::Change) {
            state.unwrap_or_else(|| "object=unknown, projection=reference-only".into())
        } else {
            String::new()
        };
        node.summary = Some(format!(
            "[{role}] {evidence} {}",
            node.summary.as_deref().unwrap_or("")
        ));
    }
}

pub(crate) fn render_text(graph: &Graph) -> String {
    render_tree(graph, false)
}

fn render_details(graph: &Graph) -> String {
    render_tree(graph, true)
}

fn render_tree(graph: &Graph, details: bool) -> String {
    use std::collections::BTreeMap;

    let root = graph.nodes.iter().find(|node| node.id == graph.root);
    let mut out = match root {
        Some(node) => {
            let key = metadata_text(node, "human_key");
            let label = tree_label(node);
            format!(
                "Intent{} — {label}\n",
                key.map(|s| format!(" {s}")).unwrap_or_default()
            )
        }
        None => "Intent [root missing]\n".into(),
    };
    out.push_str(&format!(
        "{} nodes · {} implementing · {} recording · {} dependency-only{}\n",
        graph.nodes.len(),
        graph.implementing_changes.len(),
        graph.recording_changes.len(),
        graph.dependency_changes.len(),
        if graph.truncated {
            " · [TRUNCATED]"
        } else {
            ""
        },
    ));

    // These are presentation groups, not newly asserted graph edges. Each
    // selected node appears once, even when it has multiple incoming edges.
    let mut groups: BTreeMap<(u8, String), Vec<&atomic_core::KgNode>> = BTreeMap::new();
    for node in graph.nodes.iter().filter(|node| node.id != graph.root) {
        let group = if graph.implementing_changes.contains(&node.id) {
            (0, "Implementing changes".into())
        } else if graph.recording_changes.contains(&node.id) {
            (1, "Intent recording evidence (not implementation)".into())
        } else if graph.dependency_changes.contains(&node.id) {
            (2, "Dependency-only changes".into())
        } else if graph.intent_artifact.as_ref() == Some(&node.id) {
            (3, "Intent artifact".into())
        } else if node.has_type(Class::Task) {
            (4, "Tasks".into())
        } else if node.has_type(Class::AcceptanceCriterion) {
            (5, "Acceptance criteria".into())
        } else if node.has_type(Class::ScopeItem) {
            (6, "Scope".into())
        } else if node.has_type(Class::Constraint) {
            (7, "Constraints".into())
        } else if node.has_type(Class::File) {
            (8, "Files".into())
        } else {
            (
                9,
                compact(node.primary_type())
                    .trim_start_matches("atomic:")
                    .to_owned(),
            )
        };
        groups.entry(group).or_default().push(node);
    }
    let count = groups.len();
    if details {
        let mut lines = vec![
            format!("Snapshot: {}", one_line(&graph.snapshot_identity)),
            format!("Scope: {}", one_line(graph.scope)),
        ];
        if let Some(node) = root {
            lines.extend(node_details(graph, node));
        } else {
            lines.push(format!("ID: {}", one_line(&graph.root)));
        }
        write_tree_children(&mut out, "", &lines, count > 0);
    }
    for (index, ((_, title), mut nodes)) in groups.into_iter().enumerate() {
        let last_group = index + 1 == count;
        let branch = if last_group { "└──" } else { "├──" };
        let indent = if last_group { "    " } else { "│   " };
        nodes.sort_by(|a, b| (&a.label, &a.id).cmp(&(&b.label, &b.id)));
        out.push_str(&format!("{branch} {title} ({})\n", nodes.len()));
        for (i, node) in nodes.iter().enumerate() {
            let branch = if i + 1 == nodes.len() {
                "└──"
            } else {
                "├──"
            };
            let declaration = if graph.declared_changes.contains(&node.id)
                && !graph.captured_changes.contains(&node.id)
            {
                " [declared; original turn unproven]"
            } else {
                ""
            };
            let recording = if graph.recording_changes.contains(&node.id)
                && graph.implementing_changes.contains(&node.id)
            {
                " [also records intent]"
            } else {
                ""
            };
            out.push_str(&format!(
                "{indent}{branch} {}{declaration}{recording}\n",
                tree_label(node)
            ));
            if details {
                let child_indent = format!(
                    "{indent}{}",
                    if i + 1 == nodes.len() {
                        "    "
                    } else {
                        "│   "
                    }
                );
                write_tree_children(&mut out, &child_indent, &node_details(graph, node), false);
            }
        }
    }
    for id in &graph.missing_resources {
        out.push_str(&format!("Missing resource: {id}\n"));
    }
    for diagnostic in &graph.diagnostics {
        out.push_str(&format!("Note: {}\n", one_line(diagnostic)));
    }
    out.push_str(if details {
        "\nGrouped tree; relationships are references, not expanded subtrees.\n"
    } else {
        "\nGrouped tree; use --details for full labels, canonical IDs and relationships.\n"
    });
    out
}

fn write_tree_children(out: &mut String, indent: &str, lines: &[String], has_following: bool) {
    for (index, line) in lines.iter().enumerate() {
        let branch = if index + 1 == lines.len() && !has_following {
            "└──"
        } else {
            "├──"
        };
        out.push_str(&format!("{indent}{branch} {line}\n"));
    }
}

fn node_details(graph: &Graph, node: &atomic_core::KgNode) -> Vec<String> {
    let mut lines = vec![
        format!("ID: {}", one_line(&node.id)),
        format!("Type: {}", compact(node.primary_type())),
    ];
    let label = if node.has_type(Class::File) {
        metadata_text(node, "path").unwrap_or(&node.label)
    } else {
        &node.label
    };
    if console::measure_text_width(&one_line(label)) > 100 {
        lines.push(format!("Label: {}", one_line(label)));
    }
    let mut annotated = [node.clone()];
    annotate(graph, &mut annotated);
    if let Some(summary) = &annotated[0].summary {
        let summary = one_line(summary);
        let summary = summary.trim();
        // Context is the fallback role, not evidence or a useful summary.
        let summary = summary.strip_prefix("[context]").unwrap_or(summary).trim();
        if !summary.is_empty() {
            lines.push(format!("Summary: {summary}"));
        }
    }
    // References preserve graph direction without recursively expanding shared
    // targets or cycles, so details cannot change the tree's grouping.
    let mut edges: Vec<_> = graph
        .edges
        .iter()
        .filter(|edge| edge.from_id == node.id)
        .collect();
    edges.sort_by(|a, b| (a.kind, &a.to_id).cmp(&(b.kind, &b.to_id)));
    for edge in edges {
        let mut line = format!("{} → {}", compact(edge.kind.iri()), one_line(&edge.to_id));
        if let Some(evidence) = edge
            .metadata
            .as_ref()
            .and_then(|m| m.get("declarationEvidence"))
            .and_then(|v| v.as_str())
        {
            line.push_str(&format!(" [Declaration evidence: {}]", one_line(evidence)));
        }
        lines.push(line);
    }
    lines
}

fn metadata_text<'a>(node: &'a atomic_core::KgNode, key: &str) -> Option<&'a str> {
    node.metadata.as_ref()?.get(key)?.as_str()
}

fn tree_label(node: &atomic_core::KgNode) -> String {
    let label = if node.has_type(Class::File) {
        metadata_text(node, "path").unwrap_or(&node.label)
    } else {
        &node.label
    };
    let label = one_line(label);
    let status = metadata_text(node, "status")
        .map(|value| format!(" [{}]", one_line(value)))
        .unwrap_or_default();
    if node.has_type(Class::Change) {
        let id = node.id.rsplit(':').next().unwrap_or(&node.id);
        let short: String = id.chars().take(12).collect();
        if label.trim().is_empty() || label.trim() == short || label.trim() == id {
            format!("{short}{status}")
        } else {
            let label = console::truncate_str(&label, 100, "…");
            format!("{short} {label}{status}")
        }
    } else {
        let label = console::truncate_str(&label, 100, "…");
        format!("{label}{status}")
    }
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Graph {
        use atomic_core::{kg::ontology::Predicate, KgEdge, KgNode};
        let root = KgNode::new(
            "urn:intent:root",
            Class::WorkIntent,
            "Make graph readable",
            "test",
        )
        .with_metadata(serde_json::json!({"human_key": "ATOM::test::1", "status": "in_progress"}));
        let change = |id: &str, label: &str| KgNode::new(id, Class::Change, label, "test");
        Graph {
            schema_version: 1,
            root: root.id.clone(),
            snapshot_identity: "snapshot".into(),
            generation: 2,
            ontology_version: "test".into(),
            scope: "test",
            nodes: vec![
                root,
                change("urn:change:ABCDEFGHIJKLM", "Implement renderer"),
                change("urn:change:RECORD", "Record intent"),
                change("urn:change:DEPEND", "Earlier dependency"),
                KgNode::new("urn:task:1", Class::Task, "Add tests", "test")
                    .with_metadata(serde_json::json!({"status": "open"})),
                KgNode::new("urn:file:1", Class::File, "graph.rs", "test").with_metadata(
                    serde_json::json!({"path": "atomic-cli/src/commands/intent/graph.rs"}),
                ),
            ],
            edges: vec![KgEdge::new(
                "urn:change:ABCDEFGHIJKLM",
                "urn:intent:root",
                Predicate::Implements,
            )],
            implementing_changes: vec!["urn:change:ABCDEFGHIJKLM".into()],
            captured_changes: vec![],
            declared_changes: vec!["urn:change:ABCDEFGHIJKLM".into()],
            intent_artifact: None,
            recording_changes: vec!["urn:change:RECORD".into()],
            dependency_changes: vec!["urn:change:DEPEND".into()],
            truncated: false,
            missing_resources: vec![],
            diagnostics: vec![],
        }
    }

    #[test]
    fn compact_tree_separates_roles_and_uses_readable_labels() {
        assert_eq!(
            render_text(&fixture()),
            concat!(
                "Intent ATOM::test::1 — Make graph readable [in_progress]\n",
                "6 nodes · 1 implementing · 1 recording · 1 dependency-only\n",
                "├── Implementing changes (1)\n",
                "│   └── ABCDEFGHIJKL Implement renderer [declared; original turn unproven]\n",
                "├── Intent recording evidence (not implementation) (1)\n",
                "│   └── RECORD Record intent\n",
                "├── Dependency-only changes (1)\n",
                "│   └── DEPEND Earlier dependency\n",
                "├── Tasks (1)\n",
                "│   └── Add tests [open]\n",
                "└── Files (1)\n",
                "    └── atomic-cli/src/commands/intent/graph.rs\n",
                "\nGrouped tree; use --details for full labels, canonical IDs and relationships.\n",
            )
        );
    }

    #[test]
    fn change_hash_labels_are_shown_only_once() {
        use atomic_core::KgNode;
        let hash = "T5RE2LAMC244ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        for label in ["T5RE2LAMC244", hash, " T5RE2LAMC244 ", ""] {
            let node = KgNode::new(format!("urn:change:{hash}"), Class::Change, label, "test")
                .with_metadata(serde_json::json!({"status": "available"}));
            assert_eq!(tree_label(&node), "T5RE2LAMC244 [available]");
        }
        let node = KgNode::new(
            "urn:change:UNJWGUMUN3IV",
            Class::Change,
            "UNJWGUMUN3IV",
            "test",
        );
        assert_eq!(tree_label(&node), "UNJWGUMUN3IV");
        let node = KgNode::new(
            format!("urn:change:{hash}"),
            Class::Change,
            "Fix rendering",
            "test",
        );
        assert_eq!(tree_label(&node), "T5RE2LAMC244 Fix rendering");
    }

    #[test]
    fn tree_order_is_stable_and_shared_nodes_appear_once() {
        let mut graph = fixture();
        graph.nodes.push(atomic_core::KgNode::new(
            "urn:task:2",
            Class::Task,
            "A first task",
            "test",
        ));
        let expected = render_text(&graph);
        graph.nodes.reverse();
        graph.edges.reverse();
        graph.edges.extend(graph.edges.clone());
        assert_eq!(render_text(&graph), expected);
        assert_eq!(expected.matches("Add tests").count(), 1);
        assert!(expected.find("A first task").unwrap() < expected.find("Add tests").unwrap());
    }

    #[test]
    fn overlapping_change_roles_are_not_hidden_or_duplicated() {
        let mut graph = fixture();
        graph
            .recording_changes
            .push(graph.implementing_changes[0].clone());
        let text = render_text(&graph);
        assert_eq!(text.matches("Implement renderer").count(), 1);
        assert!(text.contains("[declared; original turn unproven] [also records intent]"));
        graph.captured_changes = graph.implementing_changes.clone();
        let text = render_text(&graph);
        assert!(!text.contains("original turn unproven"));
        assert!(text.contains("[also records intent]"));
    }

    #[test]
    fn tree_keeps_completeness_warnings_and_bounds_unicode_labels() {
        let mut graph = fixture();
        graph.truncated = true;
        graph.missing_resources.push("urn:missing".into());
        graph
            .diagnostics
            .push("Dependency closure incomplete".into());
        graph.nodes[4].label = format!("Task\n\u{1b}{}", "界".repeat(200));
        let text = render_text(&graph);
        assert!(text.contains("[TRUNCATED]"));
        assert!(text.contains("Missing resource: urn:missing"));
        assert!(text.contains("Note: Dependency closure incomplete"));
        assert!(text.contains("… [open]"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains(&"界".repeat(200)));
        let details = render_details(&graph);
        assert!(details.contains(&graph.nodes[1].id));
        assert!(details.contains("atomic:implements → urn:intent:root"));
        assert!(details.contains(&"界".repeat(200)));
    }

    #[test]
    fn tree_handles_root_only_and_missing_root() {
        let mut graph = fixture();
        graph.nodes.truncate(1);
        assert!(!render_text(&graph).contains("──"));
        graph.nodes.clear();
        assert!(render_text(&graph).starts_with("Intent [root missing]"));
    }

    #[test]
    fn details_expand_the_same_tree_with_nested_evidence() {
        let mut graph = fixture();
        graph.nodes[1].summary = Some("Implement the compact renderer".into());
        graph.edges[0].metadata =
            Some(serde_json::json!({"declarationEvidence": "Explicit author statement"}));
        let compact = render_text(&graph);
        let details = render_details(&graph);
        let skeleton: Vec<_> = compact
            .lines()
            .take_while(|line| !line.is_empty())
            .collect();
        let detailed_skeleton: Vec<_> = details
            .lines()
            .filter(|line| skeleton.contains(line))
            .collect();
        assert_eq!(detailed_skeleton, skeleton);
        assert!(
            details.contains("├── Snapshot: snapshot\n├── Scope: test\n├── ID: urn:intent:root\n")
        );
        assert!(details.contains("│       ├── ID: urn:change:ABCDEFGHIJKLM\n"));
        assert!(details.contains("│       └── atomic:implements → urn:intent:root [Declaration evidence: Explicit author statement]\n"));
        assert!(details
            .contains("object=unknown, projection=reference-only Implement the compact renderer"));
        assert!(details.contains(
            "    └── atomic-cli/src/commands/intent/graph.rs\n        ├── ID: urn:file:1\n"
        ));
        assert!(details.contains("        └── Type: atomic:File\n"));
        assert!(!details.contains("Summary: [context]"));
        assert!(!details.contains("use --details"));
    }

    #[test]
    fn context_summaries_require_actual_content() {
        let graph = fixture();
        let mut node = graph.nodes[5].clone();
        for summary in [None, Some(""), Some(" \n\t ")] {
            node.summary = summary.map(str::to_owned);
            assert!(!node_details(&graph, &node)
                .iter()
                .any(|line| line.starts_with("Summary:")));
        }
        node.summary = Some("Shared file used by the renderer".into());
        let lines = node_details(&graph, &node);
        assert!(lines.contains(&"Summary: Shared file used by the renderer".to_owned()));
        assert!(!lines.iter().any(|line| line.contains("[context]")));
        // Meaningful change roles and availability evidence must remain.
        let lines = node_details(&graph, &graph.nodes[2]);
        assert!(lines.iter().any(|line| line.contains("[records intent artifact; implementation attribution not asserted] object=unknown, projection=reference-only")));
    }

    #[test]
    fn details_keep_cycles_as_references_and_sort_edges() {
        use atomic_core::{kg::ontology::Predicate, KgEdge};
        let mut graph = fixture();
        graph.edges.push(KgEdge::new(
            "urn:intent:root",
            "urn:change:ABCDEFGHIJKLM",
            Predicate::DeclaresImplementation,
        ));
        graph.edges.push(KgEdge::new(
            "urn:change:ABCDEFGHIJKLM",
            "urn:file:1",
            Predicate::ModifiesFile,
        ));
        let text = render_details(&graph);
        assert_eq!(text.matches("ID: urn:change:ABCDEFGHIJKLM").count(), 1);
        assert_eq!(text.matches("ID: urn:intent:root").count(), 1);
        assert_eq!(text.matches(" → ").count(), 3);
        graph.edges.reverse();
        graph.nodes.reverse();
        assert_eq!(render_details(&graph), text);
    }

    #[test]
    fn root_only_details_close_the_tree_and_keep_warnings() {
        let mut graph = fixture();
        graph.nodes.truncate(1);
        graph.edges.clear();
        graph.truncated = true;
        graph.missing_resources.push("urn:missing".into());
        graph.diagnostics.push("Selection incomplete".into());
        let text = render_details(&graph);
        assert!(text.contains("└── Summary: [root intent]"));
        assert!(text.contains("[TRUNCATED]"));
        assert!(text.contains("Missing resource: urn:missing"));
        assert!(text.contains("Note: Selection incomplete"));
    }

    #[test]
    fn details_is_a_text_only_option() {
        assert!(
            IntentGraph::try_parse_from(["graph", "ATOM-1", "--details"])
                .unwrap()
                .details
        );
        for export in ["--json", "--dot", "--html"] {
            assert!(IntentGraph::try_parse_from(["graph", "ATOM-1", "--details", export]).is_err());
        }
    }

    #[test]
    fn html_is_complete_before_browser_opens_and_file_url_is_encoded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("intent #1 100%.html");
        let html = "<!DOCTYPE html><title>Intent graph</title>";
        let mut called = false;
        let written = write_and_open_html(html, Some(&path), |target| {
            called = true;
            let url = url::Url::parse(target).unwrap();
            assert_eq!(url.scheme(), "file");
            assert!(target.contains("%23") && target.contains("%25") && target.contains("%20"));
            assert_eq!(
                // Windows file URLs decode without the verbatim `\\?\`
                // prefix returned by canonicalize. Compare canonical paths on
                // both sides, not two valid spellings of the same file.
                std::fs::canonicalize(url.to_file_path().unwrap()).unwrap(),
                std::fs::canonicalize(&path).unwrap()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), html);
            Ok(())
        })
        .unwrap();
        assert!(called);
        assert_eq!(written, std::fs::canonicalize(path).unwrap());
    }

    #[test]
    fn automatic_html_files_are_unique_and_survive_browser_failure() {
        let first = write_and_open_html("first graph", None, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no browser available",
            ))
        })
        .unwrap();
        let second = write_and_open_html("second graph", None, |_| Ok(())).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.extension().unwrap(), "html");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first graph");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second graph");
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
    }

    #[test]
    fn failed_html_write_does_not_launch_browser() {
        let directory = tempfile::tempdir().unwrap();
        let mut called = false;
        assert!(write_and_open_html("graph", Some(directory.path()), |_| {
            called = true;
            Ok(())
        })
        .is_err());
        assert!(!called);
    }
}
