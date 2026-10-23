//! `atomic intent graph <ID>` — semantic lineage, not a search neighborhood.

use crate::{
    commands::Command,
    error::{CliError, CliResult},
};
use atomic_core::kg::ontology::compact;
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
        } else {
            render_text(&graph)
        };
        for diagnostic in &graph.diagnostics {
            eprintln!("Note: {diagnostic}");
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
    let mut out = format!("Intent semantic change graph\nRoot: {}\nSnapshot: {}\nScope: {}\n{} nodes, {} edges; {} implementing changes, {} intent-artifact recording changes, {} dependency-only changes{}\n\n", graph.root, graph.snapshot_identity, graph.scope, graph.nodes.len(), graph.edges.len(), graph.implementing_changes.len(), graph.recording_changes.len(), graph.dependency_changes.len(), if graph.truncated { " [TRUNCATED]" } else { "" });
    let mut nodes = graph.nodes.clone();
    annotate(graph, &mut nodes);
    for node in &nodes {
        out.push_str(&format!(
            "[{}] {}\n  {}\n",
            compact(node.primary_type()),
            one_line(&node.label),
            node.id
        ));
        if let Some(summary) = &node.summary {
            out.push_str(&format!("  {}\n", one_line(summary)));
        }
        for edge in graph.edges.iter().filter(|e| e.from_id == node.id) {
            out.push_str(&format!(
                "  --{}--> {}\n",
                compact(edge.kind.iri()),
                edge.to_id
            ));
            if let Some(evidence) = edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("declarationEvidence"))
                .and_then(|v| v.as_str())
            {
                out.push_str(&format!(
                    "    Declaration evidence: {}\n",
                    one_line(evidence)
                ));
            }
        }
    }
    for id in &graph.missing_resources {
        out.push_str(&format!("MISSING: {id}\n"));
    }
    for diagnostic in &graph.diagnostics {
        out.push_str(&format!("Note: {diagnostic}\n"));
    }
    out
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
                url.to_file_path().unwrap(),
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
