//! `atomic vault query` — query the vault knowledge graph.

use std::collections::{HashMap, HashSet};

use clap::{Parser, Subcommand};

use atomic_core::kg::ontology::{compact, Class};
use atomic_core::pristine::vault::{KgEdge, KgNode};
use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{create_spinner, finish_error, finish_success};
mod validate;

/// Subcommands for vault knowledge graph queries.
#[derive(Subcommand, Debug)]
pub enum QueryCommands {
    /// Evaluate builtin and local enterprise SHACL shapes over a workflow snapshot.
    Validate(validate::Validate),
    /// Search the knowledge graph by keywords.
    ///
    /// Performs full-text search over KG node IDs, labels, and summaries,
    /// returning matching nodes ranked by relevance. VCS nodes and source
    /// content are filtered to the current view; vault-only nodes are shared.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query search "authentication"
    /// atomic vault query search "auth" -k 20
    /// atomic vault query search "architecture" --json
    /// ```
    Search(QueryNodes),

    /// Get the neighborhood of a KG node.
    ///
    /// Returns the subgraph around a node: all directly connected
    /// nodes and edges (up to the specified depth).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query neighbors "goal:swift-meadow"
    /// atomic vault query neighbors "intent:PIMO-1" -d 2
    /// atomic vault query neighbors "file:src/auth.rs" --json
    /// ```
    Neighbors(QueryNeighbors),

    /// Find all functions that call a given entity.
    ///
    /// Resolves calls in the current view's recorded source whose destination
    /// matches a path-qualified or bare symbol. Useful for blast-radius analysis: see every
    /// function that breaks if this function's signature changes.
    ///
    /// Run `atomic vault query entities <file>` to discover symbol names.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query callers 'src/auth.rs#verify'
    /// atomic vault query callers verify --json
    /// ```
    Callers(QueryCallers),

    /// List tree-sitter entities (functions, classes, types) in a file.
    ///
    /// Parses the file with tree-sitter and prints all extracted entities.
    /// Requires the `ast` feature to be enabled at build time.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query entities src/main.rs
    /// atomic vault query entities src/main.rs --json
    /// ```
    Entities(QueryEntities),

    /// Build or update the content search index.
    ///
    /// Builds a syntext n-gram index over all source files in the repository.
    /// The index is stored at `.atomic/content-index/` for working-copy content
    /// consumers. View-scoped `query code` does not require it.
    ///
    /// Without `--rebuild`, performs an incremental update (fast).
    /// With `--rebuild`, does a full rebuild from scratch.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query index
    /// atomic vault query index --rebuild
    /// atomic vault query index --stats
    /// ```
    Index(QueryIndex),

    /// Search recorded source code in the current view.
    ///
    /// Searches the view's ancestor/dependency closure, excluding sibling-view
    /// and unrecorded working-copy content. No content-index rebuild is needed.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query code "replication"
    /// atomic vault query code "replication" -g "src/mongo/db/repl/"
    /// atomic vault query code "replication" -t cpp
    /// atomic vault query code "fn parse_query" --json
    /// ```
    Code(QueryCode),

    /// Build a visual graph from a search query.
    ///
    /// Searches the KG, expands results via neighbor traversal, and emits
    /// the resulting subgraph in DOT (Graphviz) format. Pipe to `dot` to
    /// render:
    ///
    ///   atomic vault query graph "replication" | dot -Tsvg -o graph.svg
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query graph "replication"
    /// atomic vault query graph "replication" -k 10 --depth 2
    /// atomic vault query graph "replication" --json
    /// ```
    Graph(QueryGraph),

    /// Rebuild embeddings for vault content.
    ///
    /// Embeddings are computed automatically on vault writes.
    /// Use this command to rebuild after changing the embedding provider
    /// or to recover from corruption.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query embed
    /// atomic vault query embed -p memory/architecture.md
    /// ```
    Embed(QueryEmbed),

    /// Enrich the KG from VCS data (changes, files, views).
    ///
    /// VCS data is enriched into the KG automatically after each record.
    /// Pass `--change` to materialize exact recorded changes, or omit it to do
    /// a full rebuild.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query enrich
    /// atomic vault query enrich --change <full-change-hash>
    /// ```
    Enrich(QueryEnrich),

    /// Rebuild the KG index from all vault entries.
    ///
    /// The KG is indexed automatically on vault writes.
    /// Use this command to rebuild from scratch.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query reindex
    /// ```
    Reindex(Reindex),

    /// Execute a structured query plan (JSON from stdin).
    ///
    /// Reads a JSON query plan from stdin and executes each step
    /// against the knowledge graph. Use `--json` for machine-readable output.
    ///
    /// # Examples
    ///
    /// ```text
    /// echo '{"steps":[{"type":"kg_search","query":"auth","limit":5}]}' | atomic vault query plan
    /// echo '{"steps":[...]}' | atomic vault query plan --json
    /// ```
    Plan(PlanExec),

    /// Ask a question using the knowledge graph (RAG).
    ///
    /// Searches the KG for relevant nodes and edges, builds a context
    /// string, and calls an LLM to generate an answer. Falls back to
    /// search-only mode when no API key is configured.
    ///
    /// Requires `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` to be set for
    /// LLM answers. Without an API key, displays search results only.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query ask "who fixed the auth bug?"
    /// atomic vault query ask "what does the payment service depend on?" -k 20
    /// atomic vault query ask "summarize recent changes" --json
    /// ```
    Ask(QueryAsk),
}

/// Query the vault knowledge graph.
#[derive(Debug, clap::Args)]
pub struct Query {
    #[command(subcommand)]
    pub command: QueryCommands,
}

impl Command for Query {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            QueryCommands::Validate(cmd) => cmd.run(),
            QueryCommands::Search(cmd) => cmd.run(),
            QueryCommands::Neighbors(cmd) => cmd.run(),
            QueryCommands::Callers(cmd) => cmd.run(),
            QueryCommands::Entities(cmd) => cmd.run(),
            QueryCommands::Index(cmd) => cmd.run(),
            QueryCommands::Code(cmd) => cmd.run(),
            QueryCommands::Graph(cmd) => cmd.run(),
            QueryCommands::Embed(cmd) => cmd.run(),
            QueryCommands::Reindex(cmd) => cmd.run(),
            QueryCommands::Enrich(cmd) => cmd.run(),
            QueryCommands::Ask(cmd) => cmd.run(),
            QueryCommands::Plan(cmd) => cmd.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// QueryGraph — build a subgraph from a search query and visualize it
// ---------------------------------------------------------------------------

/// Build a visual graph from a search query.
#[derive(Parser, Debug)]
pub struct QueryGraph {
    /// Search query to seed the graph.
    pub query: String,

    /// Maximum seed nodes from search.
    #[arg(long, short = 'k', default_value = "10")]
    pub limit: usize,

    /// Neighbor expansion depth (1 or 2).
    #[arg(long, default_value = "1")]
    pub depth: u8,

    /// Write output to a file. For HTML output (default), use .html extension.
    /// If omitted, writes to stdout (DOT or JSON) or a temp file (HTML) and opens it.
    #[arg(long, short = 'o')]
    pub output: Option<String>,

    /// Output as JSON (nodes + edges arrays).
    #[arg(long)]
    pub json: bool,

    /// Output as DOT (Graphviz) format instead of HTML.
    #[arg(long)]
    pub dot: bool,

    /// Maximum nodes in the graph (default: 5000 for HTML, 200 for DOT).
    #[arg(long)]
    pub max_nodes: Option<usize>,

    /// Node kinds to include (comma-separated). Default: all.
    /// Examples: "module,file,entity" for structure only, "change,file" for history.
    #[arg(long, default_value = "all")]
    pub kinds: String,

    /// Maximum change nodes to keep per seed during expansion.
    /// Prevents change history from flooding the graph while still showing
    /// the most connected changes. Default: 5. Use 0 for unlimited.
    #[arg(long, default_value = "5")]
    pub changes_per_seed: usize,
}

/// Default node cap for DOT output (Graphviz chokes on large graphs).
const MAX_GRAPH_NODES_DOT: usize = 200;

/// Default node cap for HTML output (D3 canvas handles thousands).
const MAX_GRAPH_NODES_HTML: usize = 5000;

impl Command for QueryGraph {
    fn run(&self) -> CliResult<()> {
        // Route every form through the service layer: the seed/expand/
        // filter/cap build runs handler-side; the DOT/HTML/JSON renders
        // stay client-side over the wire-carried subgraph.
        if crate::commands::rpc::query_graph(self)? {
            return Ok(());
        }
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let depth = self.depth.min(2);

        // Parse allowed kinds filter
        let allow_all = self.kinds.trim().eq_ignore_ascii_case("all");
        let allowed_kinds: HashSet<String> = if allow_all {
            HashSet::new() // empty = no filter
        } else {
            self.kinds
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let kind_allowed = |kind: &str| -> bool { allow_all || allowed_kinds.contains(kind) };

        // 1. Seed: search the KG in this working copy's desired view,
        //    filtered to allowed kinds. VCS nodes and source content come
        //    from the view's recorded closure; vault-only nodes stay shared.
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        let view = repo
            .desired_view_name(working_copy)
            .map_err(CliError::Repository)?;
        let seed_nodes: Vec<KgNode> = repo
            .vault_kg_search_on_view(&view, &self.query, self.limit * 3, None) // fetch extra, then filter
            .map_err(CliError::Repository)?
            .into_iter()
            .filter(|n| n.types.iter().any(|class| kind_allowed(class)))
            .take(self.limit)
            .collect();

        if seed_nodes.is_empty() && !self.json {
            eprintln!("No results for {:?} (kinds: {}).", self.query, self.kinds);
            return Ok(());
        }

        // 2. Expand: gather neighbor subgraphs, filtering to allowed kinds.
        //    Change nodes are capped per seed to prevent history from flooding
        //    the graph while still showing the most relevant changes.
        let mut node_map: HashMap<String, KgNode> = HashMap::new();
        let mut edge_set: HashSet<(String, String, String)> = HashSet::new();
        let mut all_edges: Vec<KgEdge> = Vec::new();
        let cap_changes = self.changes_per_seed > 0;

        for seed in &seed_nodes {
            node_map
                .entry(seed.id.clone())
                .or_insert_with(|| seed.clone());

            let subgraph = repo
                .vault_kg_neighbors(&seed.id, depth)
                .map_err(CliError::Repository)?;

            // Count change nodes added for this seed so we can cap them
            let mut changes_for_seed: usize = 0;

            for node in subgraph.nodes {
                if !node.types.iter().any(|class| kind_allowed(class)) {
                    continue;
                }
                // Cap change nodes per seed
                if node.has_type(Class::Change) && cap_changes {
                    if node_map.contains_key(&node.id) {
                        continue; // already added by another seed, don't count
                    }
                    if changes_for_seed >= self.changes_per_seed {
                        continue; // hit the cap for this seed
                    }
                    changes_for_seed += 1;
                }
                node_map.entry(node.id.clone()).or_insert(node);
            }

            for edge in subgraph.edges {
                let from_kind_ok = node_map.contains_key(&edge.from_id);
                let to_kind_ok = node_map.contains_key(&edge.to_id);
                if from_kind_ok && to_kind_ok {
                    // Only add edge if both endpoints made it into node_map
                    // (respects the per-seed change cap)
                    if node_map.contains_key(&edge.from_id) || node_map.contains_key(&edge.to_id) {
                        let key = (
                            edge.from_id.clone(),
                            edge.to_id.clone(),
                            edge.kind.to_string(),
                        );
                        if edge_set.insert(key) {
                            all_edges.push(edge);
                        }
                    }
                }
            }
        }

        // 3. Remove isolated nodes (no edges) — they're noise, not a graph.
        //    A node is connected if it appears as from_id or to_id in any edge.
        let connected_ids: HashSet<String> = all_edges
            .iter()
            .flat_map(|e| [e.from_id.clone(), e.to_id.clone()])
            .collect();

        // Keep seed nodes even if isolated (they're what the user searched for),
        // but drop non-seed nodes that have no edges.
        let seed_ids: HashSet<&str> = seed_nodes.iter().map(|n| n.id.as_str()).collect();
        let node_map_filtered: HashMap<String, KgNode> = node_map
            .into_iter()
            .filter(|(id, _)| connected_ids.contains(id) || seed_ids.contains(id.as_str()))
            .collect();

        let mut nodes: Vec<KgNode> = node_map_filtered.into_values().collect();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));

        // Only retain edges where both endpoints survived filtering.
        let final_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        all_edges.retain(|e| {
            final_ids.contains(e.from_id.as_str()) && final_ids.contains(e.to_id.as_str())
        });

        // Cap at max_nodes (user flag, or default based on output format)
        let max_nodes = self.max_nodes.unwrap_or(if self.dot {
            MAX_GRAPH_NODES_DOT
        } else {
            MAX_GRAPH_NODES_HTML
        });

        if nodes.len() > max_nodes {
            eprintln!(
                "Note: {} nodes collected, showing top {}. Use --max-nodes to adjust.",
                nodes.len(),
                max_nodes
            );
            let kept_ids: HashSet<&str> =
                nodes[..max_nodes].iter().map(|n| n.id.as_str()).collect();
            all_edges.retain(|e| {
                kept_ids.contains(e.from_id.as_str()) && kept_ids.contains(e.to_id.as_str())
            });
            nodes.truncate(max_nodes);
        }

        // Drop any nodes that became isolated after capping
        if nodes.len() > self.limit {
            let connected_after_cap: HashSet<String> = all_edges
                .iter()
                .flat_map(|e| [e.from_id.clone(), e.to_id.clone()])
                .collect();
            nodes.retain(|n| {
                connected_after_cap.contains(&n.id) || seed_ids.contains(n.id.as_str())
            });
        }

        repo.enrich_workflow_usage(&mut nodes)
            .map_err(CliError::Repository)?;

        // 4. Output
        if self.json {
            let output = serde_json::json!({
                "nodes": nodes,
                "edges": all_edges,
            });
            let json_str = serde_json::to_string_pretty(&output).unwrap();
            if let Some(ref path) = self.output {
                std::fs::write(path, &json_str)
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("write failed: {e}")))?;
                eprintln!(
                    "Wrote {} nodes, {} edges to {}",
                    nodes.len(),
                    all_edges.len(),
                    path
                );
            } else {
                println!("{}", json_str);
            }
        } else if self.dot {
            let dot_str = emit_dot(&nodes, &all_edges);
            if let Some(ref path) = self.output {
                std::fs::write(path, &dot_str)
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("write failed: {e}")))?;
                eprintln!(
                    "Wrote {} nodes, {} edges to {}",
                    nodes.len(),
                    all_edges.len(),
                    path
                );
            } else {
                print!("{}", dot_str);
            }
        } else {
            // Default: HTML with embedded D3 force-directed graph
            let html = emit_html(&self.query, &nodes, &all_edges);
            if let Some(ref path) = self.output {
                std::fs::write(path, &html)
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("write failed: {e}")))?;
                eprintln!(
                    "Wrote {} nodes, {} edges to {}",
                    nodes.len(),
                    all_edges.len(),
                    path
                );
            } else {
                // Write to temp file and open in browser
                let tmp_dir = std::env::temp_dir();
                let tmp_path = tmp_dir.join("atomic-graph.html");
                std::fs::write(&tmp_path, &html)
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("write failed: {e}")))?;
                eprintln!(
                    "Opening graph ({} nodes, {} edges) in browser...",
                    nodes.len(),
                    all_edges.len()
                );
                let _ = std::process::Command::new("open").arg(&tmp_path).status();
            }
        }

        Ok(())
    }
}

/// Generate a DOT (Graphviz) representation of a subgraph.
pub(crate) fn emit_dot(nodes: &[KgNode], edges: &[KgEdge]) -> String {
    let mut out = String::new();
    out.push_str("digraph query {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str(
        "  node [shape=box, style=\"filled,rounded\", fontname=\"Helvetica\", fontsize=10];\n",
    );
    out.push_str("  edge [fontname=\"Helvetica\", fontsize=8, color=\"#888888\"];\n");
    out.push('\n');

    // Nodes
    for node in nodes {
        let (fillcolor, shape) = node_style(node.primary_type());
        let label = node_label(node);
        out.push_str(&format!(
            "  \"{}\" [label=\"{}\", fillcolor=\"{}\", shape={}];\n",
            dot_escape_id(&node.id),
            dot_escape_label(&label),
            fillcolor,
            shape,
        ));
    }

    out.push('\n');

    // Edges
    for edge in edges {
        out.push_str(&format!(
            "  \"{}\" -> \"{}\" [label=\"{}\"];\n",
            dot_escape_id(&edge.from_id),
            dot_escape_id(&edge.to_id),
            dot_escape_label(&compact(edge.kind.iri())),
        ));
    }

    out.push_str("}\n");
    out
}

/// Returns `(fillcolor, shape)` for a given node kind.
fn node_style(kind: &str) -> (&'static str, &'static str) {
    match Class::from_iri(kind) {
        Some(Class::File) => ("#d5f5e3", "box"),
        Some(Class::Change) => ("#fadbd8", "note"),
        Some(Class::View) => ("#e8daef", "pentagon"),
        Some(
            Class::Goal
            | Class::WorkIntent
            | Class::FeatureIntent
            | Class::BugIntent
            | Class::ReviewIntent,
        ) => ("#d1f2eb", "hexagon"),
        Some(Class::Memory | Class::Identity) => ("#fef9e7", "box"),
        _ => ("#f2f3f4", "box"),
    }
}

/// Build a human-readable label for a node in DOT output.
fn node_label(node: &KgNode) -> String {
    let kind = compact(node.primary_type());
    match kind.as_str() {
        "module" => {
            // Last path component + trailing slash
            let last = node
                .label
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(&node.label);
            format!("{}/", last)
        }
        "atomic:File" => {
            // Filename, optionally with match count
            let filename = node.label.rsplit('/').next().unwrap_or(&node.label);
            if let Some(ref meta) = node.metadata {
                if let Some(matches) = meta.get("content_matches").and_then(|v| v.as_u64()) {
                    return format!("{}\n{} matches", filename, matches);
                }
            }
            filename.to_string()
        }
        "entity" => {
            // Label + kind/line info from metadata
            let mut label = node.label.clone();
            if let Some(ref meta) = node.metadata {
                let ent_kind = meta.get("kind").and_then(|v| v.as_str());
                let line = meta.get("line").and_then(|v| v.as_u64());
                let end_line = meta.get("end_line").and_then(|v| v.as_u64());
                if let (Some(k), Some(l)) = (ent_kind, line) {
                    if let Some(el) = end_line {
                        label = format!("{}\n{} L{}-{}", label, k, l, el);
                    } else {
                        label = format!("{}\n{} L{}", label, k, l);
                    }
                }
            }
            label
        }
        "atomic:Change" => {
            // Short hash + optional summary excerpt
            let mut label = node.label.clone();
            if let Some(ref summary) = node.summary {
                let excerpt: String = summary.chars().take(30).collect();
                label = format!("{}\n{}", label, excerpt);
            }
            label
        }
        _ => {
            // Everything else: label truncated to 40 chars
            if node.label.len() > 40 {
                let truncated: String = node.label.chars().take(37).collect();
                format!("{}...", truncated)
            } else {
                node.label.clone()
            }
        }
    }
}

/// Escape a string for use as a DOT node ID (inside quotes).
fn dot_escape_id(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn dot_escape_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Stable categorical colors with contrast against the HTML graph's dark canvas.
/// Exhaustive matching keeps ontology additions from silently sharing a fallback.
fn html_node_color(kind: &str) -> String {
    let Some(class) = Class::from_iri(kind) else {
        // Unlike a palette indexed by the types currently visible, hashing the
        // full IRI keeps an external type's color stable between exports.
        let hash = kind.bytes().fold(2166136261u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(16777619)
        });
        return format!("hsl({}, 65%, 68%)", hash % 360);
    };
    match class {
        Class::WorkIntent => "#63d5ea",
        Class::FeatureIntent => "#56cfe1",
        Class::BugIntent => "#ff8a9a",
        Class::ReviewIntent => "#a5b4fc",
        Class::ManagedIntent => "#8be4f0",
        Class::Goal => "#6ed4ae",
        Class::Memory => "#e2bc81",
        Class::Skill => "#a4d98b",
        Class::ToolResult => "#8da9bd",
        Class::Task => "#dd81ef",
        Class::AcceptanceCriterion => "#c6e96e",
        Class::ScopeItem => "#c5a582",
        Class::Constraint => "#f59ac2",
        Class::Todo => "#d79e64",
        Class::Session => "#b6a4ff",
        Class::SessionTurn => "#39b8bd",
        Class::ManagedTurn => "#8ad4dc",
        Class::ProvenanceGraph => "#72a7ff",
        Class::Attestation => "#f0c4ec",
        Class::Finding => "#ffad66",
        Class::Change => "#ff786f",
        Class::View => "#9878e8",
        Class::File => "#53d89b",
        Class::Identity => "#f4d06f",
        Class::Concept => "#aac4a0",
        Class::Project => "#adcbe7",
        Class::ProjectIdentity => "#bba2a0",
        Class::VerificationStatus => "#d4d8e3",
    }
    .to_owned()
}

/// Generate a self-contained HTML page with a D3.js force-directed graph.
pub(crate) fn emit_html(query: &str, nodes: &[KgNode], edges: &[KgEdge]) -> String {
    // Build JSON data for embedding
    let graph_nodes: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| {
            let content_matches = n
                .metadata
                .as_ref()
                .and_then(|m| m.get("content_matches"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let entity_kind = n
                .metadata
                .as_ref()
                .and_then(|m| m.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let line = n
                .metadata
                .as_ref()
                .and_then(|m| m.get("line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let end_line = n
                .metadata
                .as_ref()
                .and_then(|m| m.get("end_line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            serde_json::json!({
                "id": n.id,
                "kind": n.primary_type(),
                "color": html_node_color(n.primary_type()),
                "types": n.types,
                "label": n.label,
                "summary": n.summary.as_deref().unwrap_or(""),
                "source": n.source,
                "metadata": n.metadata,
                "content_matches": content_matches,
                "entity_kind": entity_kind,
                "line": line,
                "end_line": end_line,
            })
        })
        .collect();

    let graph_edges: Vec<serde_json::Value> = edges
        .iter()
        .map(|e| {
            serde_json::json!({
                "source": e.from_id,
                "target": e.to_id,
                "kind": e.kind,
                "metadata": e.metadata,
            })
        })
        .collect();

    let graph_data = serde_json::json!({
        "nodes": graph_nodes,
        "links": graph_edges,
    });

    let data_json = serde_json::to_string(&graph_data)
        .unwrap()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let escaped_query = query
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Atomic Graph: {query}</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ background: #1a1a2e; color: #e0e0e0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif; overflow: hidden; }}
  #header {{ position: fixed; top: 0; left: 0; right: 0; z-index: 10; padding: 12px 20px; background: rgba(26,26,46,0.95); border-bottom: 1px solid #333; display: flex; align-items: center; gap: 16px; }}
  #header h1 {{ font-size: 14px; font-weight: 600; color: #a0a0c0; letter-spacing: 1px; }}
  #header .query {{ font-size: 14px; color: #6ec6ff; }}
  #header .stats {{ font-size: 12px; color: #888; margin-left: auto; }}
  canvas {{ display: block; width: 100%; height: 100%; }}
  #tooltip {{ position: fixed; padding: 10px 14px; background: rgba(0,0,0,0.92); border: 1px solid #555; border-radius: 6px; font-size: 12px; pointer-events: none; display: none; z-index: 20; max-width: 420px; line-height: 1.5; }}
  #tooltip .kind {{ color: #888; font-size: 10px; text-transform: uppercase; letter-spacing: 0.5px; }}
  #tooltip .label {{ color: #fff; font-weight: 600; margin: 3px 0; word-break: break-all; }}
  #tooltip .summary {{ color: #bbb; font-size: 11px; margin-bottom: 2px; }}
  #tooltip .detail {{ color: #aaa; font-size: 11px; }}
  #legend {{ position: absolute; bottom: 16px; right: 16px; z-index: 10; background: rgba(26,26,46,0.92); border: 1px solid #333; border-radius: 8px; padding: 10px 14px; font-size: 11px; }}
  #legend .legend-title {{ color: #a0a0c0; font-weight: 600; font-size: 10px; text-transform: uppercase; letter-spacing: 0.5px; margin-bottom: 6px; }}
  #legend .legend-item {{ display: flex; align-items: center; gap: 8px; margin: 3px 0; color: #ccc; }}
  #legend .legend-dot {{ width: 10px; height: 10px; border-radius: 50%; flex-shrink: 0; }}
  {inspector_css}
</style>
</head>
<body>
<div id="header">
  <h1>ATOMIC GRAPH</h1>
  <span class="query">"{query}"</span>
  <span class="stats" id="stats"></span>
</div>
<div id="tooltip">
  <div class="kind" id="tt-kind"></div>
  <div class="label" id="tt-label"></div>
  <div class="summary" id="tt-summary"></div>
  <div class="detail" id="tt-detail"></div>
</div>
<div id="graph-layout">
  <div id="graph-view">
    <canvas id="graph"></canvas>
    <div id="legend"></div>
  </div>
  <aside id="node-inspector" aria-label="Node details"></aside>
</div>
<script src="https://cdn.jsdelivr.net/npm/d3@7"></script>
<script>
(function() {{
  {inspector_js}
  // Embedded graph data
  var graphData = {data_json};

  var kindColors = new Map(graphData.nodes.map(function(n) {{ return [n.kind, n.color]; }}));
  var EDGE_COLORS = {{
    'https://atomic.dev/ns/workflow#modifiesFile': '#e74c3c',
    'https://atomic.dev/ns/workflow#authoredBy': '#f1c40f',
    'https://atomic.dev/ns/workflow#directlyDependsOnChange': '#e74c3c',
    'https://atomic.dev/ns/workflow#directlyOnView': '#9b59b6',
    'https://atomic.dev/ns/workflow#references': '#95a5a6',
  }};

  function nodeRadius(d) {{
    if (d.kind === 'https://atomic.dev/ns/workflow#File') return 5 + Math.min(d.content_matches, 100) * 0.12;
    if (d.kind === 'https://atomic.dev/ns/workflow#Change') return 4;
    return 5;
  }}

  function nodeColor(d) {{ return d.color; }}
  function edgeColor(d) {{ return EDGE_COLORS[d.kind] || '#555'; }}

  // Canvas setup
  var canvas = document.getElementById('graph');
  var graphView = document.getElementById('graph-view');
  var ctx = canvas.getContext('2d');
  var dpr = window.devicePixelRatio || 1;
  var width = graphView.clientWidth;
  var height = graphView.clientHeight;
  canvas.width = width * dpr;
  canvas.height = height * dpr;
  canvas.style.width = width + 'px';
  canvas.style.height = height + 'px';
  ctx.scale(dpr, dpr);

  // Build node and link arrays
  var nodes = graphData.nodes.map(function(n) {{
    return Object.assign({{}}, n, {{
      x: width / 2 + (Math.random() - 0.5) * 400,
      y: height / 2 + (Math.random() - 0.5) * 400,
      r: 0,
    }});
  }});
  nodes.forEach(function(n) {{ n.r = nodeRadius(n); }});

  var nodeById = new Map(nodes.map(function(n) {{ return [n.id, n]; }}));
  var selectedNodeId = null;

  var links = graphData.links.filter(function(l) {{
    return nodeById.has(l.source) && nodeById.has(l.target);
  }});

  document.getElementById('stats').textContent = nodes.length + ' nodes \u00b7 ' + links.length + ' edges';

  // Build legend from kinds present in data
  var kindsPresent = new Set(nodes.map(function(n) {{ return n.kind; }}));
  var legendEl = document.getElementById('legend');
  var legendTitle = document.createElement('div');
  legendTitle.className = 'legend-title';
  legendTitle.textContent = 'Node Types';
  legendEl.appendChild(legendTitle);
  var kindOrder = Array.from(kindsPresent).sort();
  kindOrder.forEach(function(k) {{
    if (kindsPresent.has(k)) {{
      var item = document.createElement('div');
      item.className = 'legend-item';
      item.dataset.kind = k;
      var dot = document.createElement('span');
      dot.className = 'legend-dot';
      dot.style.background = kindColors.get(k);
      item.appendChild(dot);
      item.appendChild(document.createTextNode(k.replace('https://atomic.dev/ns/workflow#', 'atomic:')));
      legendEl.appendChild(item);
    }}
  }});
  var inspector = createGraphInspector(document.getElementById('node-inspector'), graphData, function(id) {{
    selectedNodeId = id;
    hideTooltip();
    draw();
  }});

  // Current zoom transform
  var currentTransform = d3.zoomIdentity;

  // d3-force simulation with Barnes-Hut
  var simulation = d3.forceSimulation(nodes)
    .force('link', d3.forceLink(links).id(function(d) {{ return d.id; }}).distance(80))
    .force('charge', d3.forceManyBody().strength(-30).distanceMax(300))
    .force('center', d3.forceCenter(width / 2, height / 2))
    .force('collide', d3.forceCollide().radius(function(d) {{ return d.r + 2; }}))
    .on('tick', draw);

  // Drawing function
  function draw() {{
    ctx.save();
    ctx.clearRect(0, 0, width, height);
    ctx.translate(currentTransform.x, currentTransform.y);
    ctx.scale(currentTransform.k, currentTransform.k);

    // Draw edges
    ctx.lineWidth = 0.6;
    ctx.globalAlpha = 0.4;
    links.forEach(function(l) {{
      var s = l.source;
      var t = l.target;
      var highlighted = selectedNodeId && (s.id === selectedNodeId || t.id === selectedNodeId);
      ctx.globalAlpha = selectedNodeId ? (highlighted ? 0.9 : 0.12) : 0.4;
      ctx.lineWidth = highlighted ? 1.8 : 0.6;
      ctx.strokeStyle = edgeColor(l);
      ctx.beginPath();
      ctx.moveTo(s.x, s.y);
      ctx.lineTo(t.x, t.y);
      ctx.stroke();
    }});
    ctx.globalAlpha = 1.0;

    // Draw nodes
    nodes.forEach(function(d) {{
      var r = d.r;
      ctx.beginPath();
      ctx.arc(d.x, d.y, r, 0, 2 * Math.PI);
      ctx.fillStyle = nodeColor(d);
      ctx.globalAlpha = 0.9;
      ctx.fill();
      ctx.strokeStyle = '#222';
      ctx.lineWidth = 1.2;
      ctx.globalAlpha = 1.0;
      ctx.stroke();
      if (d.id === selectedNodeId) {{
        ctx.beginPath();
        ctx.arc(d.x, d.y, r + 4 / currentTransform.k, 0, 2 * Math.PI);
        ctx.strokeStyle = '#9fd6ff';
        ctx.lineWidth = 2 / currentTransform.k;
        ctx.stroke();
      }}
    }});

    // Draw labels for large nodes (modules and high-match files)
    ctx.globalAlpha = 0.85;
    ctx.fillStyle = '#ccc';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'top';
    nodes.forEach(function(d) {{
      if (d.r > 10 || d.id === selectedNodeId) {{
        var label = d.label;
        if (d.kind === 'module') label += '/';
        if (label.length > 28) label = label.slice(0, 25) + '...';
        var fontSize = Math.max(9, Math.min(12, d.r * 0.7));
        ctx.font = fontSize + 'px -apple-system, BlinkMacSystemFont, sans-serif';
        ctx.fillText(label, d.x, d.y + d.r + 3);
      }}
    }});
    ctx.globalAlpha = 1.0;

    ctx.restore();
  }}

  // Zoom and pan via d3.zoom
  var zoomBehavior = d3.zoom()
    .filter(function(event) {{
      if (event.button || (event.ctrlKey && event.type !== 'wheel')) return false;
      if (event.type === 'mousedown') {{
        dragStart = [event.clientX, event.clientY];
        dragMoved = false;
        var rect = canvas.getBoundingClientRect();
        // Let our node-drag handler own this gesture, rather than D3 starting
        // a pan and stopping propagation before the node handler can run.
        return !findNode(event.clientX - rect.left, event.clientY - rect.top);
      }}
      return true;
    }})
    .scaleExtent([0.05, 10])
    .on('zoom', function(event) {{
      currentTransform = event.transform;
      draw();
    }});

  d3.select(canvas).call(zoomBehavior);

  // Helper: transform screen coords to simulation coords
  function screenToSim(sx, sy) {{
    return [
      (sx - currentTransform.x) / currentTransform.k,
      (sy - currentTransform.y) / currentTransform.k,
    ];
  }}

  // Find node under cursor
  function findNode(sx, sy) {{
    var coords = screenToSim(sx, sy);
    var mx = coords[0];
    var my = coords[1];
    var closest = null;
    var closestDist = Infinity;
    for (var i = 0; i < nodes.length; i++) {{
      var d = nodes[i];
      var dx = d.x - mx;
      var dy = d.y - my;
      var dist = Math.sqrt(dx * dx + dy * dy);
      if (dist < d.r + 4 && dist < closestDist) {{
        closest = d;
        closestDist = dist;
      }}
    }}
    return closest;
  }}

  // Tooltip
  var tooltip = document.getElementById('tooltip');
  var ttKind = document.getElementById('tt-kind');
  var ttLabel = document.getElementById('tt-label');
  var ttSummary = document.getElementById('tt-summary');
  var ttDetail = document.getElementById('tt-detail');

  function showTooltip(d, cx, cy) {{
    ttKind.textContent = d.kind.toUpperCase();
    ttLabel.textContent = d.id;
    var summary = '';
    if (d.summary && d.summary.length > 0) {{
      summary = d.summary.length > 120 ? d.summary.slice(0, 117) + '...' : d.summary;
    }}
    ttSummary.textContent = summary;
    var detail = '';
    if (d.content_matches > 0) {{
      detail += d.content_matches + ' content matches';
    }}
    if (d.entity_kind && d.entity_kind.length > 0) {{
      detail += (detail.length > 0 ? ' \u00b7 ' : '') + d.entity_kind;
    }}
    if (d.line > 0) {{
      detail += (detail.length > 0 ? ' \u00b7 ' : '') + 'L' + d.line;
      if (d.end_line > d.line) detail += '-' + d.end_line;
    }}
    ttDetail.textContent = detail;
    tooltip.style.display = 'block';
    tooltip.style.left = (cx + 14) + 'px';
    tooltip.style.top = (cy + 14) + 'px';
  }}

  function hideTooltip() {{
    tooltip.style.display = 'none';
  }}

  // Drag state
  var dragNode = null;
  var dragSubject = null;
  var dragStart = null;
  var dragMoved = false;

  canvas.addEventListener('mousemove', function(e) {{
    if (dragStart && Math.hypot(e.clientX - dragStart[0], e.clientY - dragStart[1]) > 4) {{
      dragMoved = true;
    }}
    var rect = canvas.getBoundingClientRect();
    var sx = e.clientX - rect.left;
    var sy = e.clientY - rect.top;

    if (dragNode) {{
      var coords = screenToSim(sx, sy);
      dragNode.fx = coords[0];
      dragNode.fy = coords[1];
      simulation.alpha(0.3).restart();
      showTooltip(dragNode, e.clientX, e.clientY);
      return;
    }}

    var found = findNode(sx, sy);
    if (found) {{
      canvas.style.cursor = 'grab';
      showTooltip(found, e.clientX, e.clientY);
    }} else {{
      canvas.style.cursor = 'default';
      hideTooltip();
    }}
  }});

  canvas.addEventListener('mousedown', function(e) {{
    dragStart = [e.clientX, e.clientY];
    dragMoved = false;
    var rect = canvas.getBoundingClientRect();
    var sx = e.clientX - rect.left;
    var sy = e.clientY - rect.top;
    var found = findNode(sx, sy);
    if (found) {{
      dragNode = found;
      dragNode.fx = dragNode.x;
      dragNode.fy = dragNode.y;
      canvas.style.cursor = 'grabbing';
      // Disable zoom panning while dragging a node
      d3.select(canvas).on('.zoom', null);
      simulation.alphaTarget(0.3).restart();
      e.stopPropagation();
      e.preventDefault();
    }}
  }});

  window.addEventListener('mouseup', function() {{
    dragStart = null;
    if (dragNode) {{
      dragNode.fx = null;
      dragNode.fy = null;
      dragNode = null;
      canvas.style.cursor = 'default';
      simulation.alphaTarget(0);
      // Re-enable zoom
      d3.select(canvas).call(zoomBehavior);
    }}
  }});

  canvas.addEventListener('mouseleave', function() {{
    hideTooltip();
  }});

  canvas.addEventListener('click', function(e) {{
    if (dragMoved) return;
    var rect = canvas.getBoundingClientRect();
    var found = findNode(e.clientX - rect.left, e.clientY - rect.top);
    inspector.select(found ? found.id : null);
  }});

  // Handle resize
  function resizeGraph() {{
    width = graphView.clientWidth;
    height = graphView.clientHeight;
    canvas.width = width * dpr;
    canvas.height = height * dpr;
    canvas.style.width = width + 'px';
    canvas.style.height = height + 'px';
    ctx.scale(dpr, dpr);
    simulation.force('center', d3.forceCenter(width / 2, height / 2));
    simulation.alpha(0.1).restart();
    draw();
  }}
  window.addEventListener('resize', resizeGraph);
  if (window.ResizeObserver) new ResizeObserver(resizeGraph).observe(graphView);
}})();
</script>
</body>
</html>"##,
        query = escaped_query,
        data_json = data_json,
        inspector_js = include_str!("graph_inspector.js"),
        inspector_css = include_str!("graph_inspector.css"),
    )
}

/// Truncate display text on character boundaries and append `...`.
pub(crate) fn truncate_display(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{}...", kept)
    }
}

/// Search the knowledge graph.
#[derive(Parser, Debug)]
pub struct QueryNodes {
    /// Search query text (keyword search over node IDs, labels, and summaries).
    pub query: String,

    /// Maximum results (0 uses the service default of 10).
    #[arg(long, short = 'k', default_value = "10")]
    pub limit: usize,

    /// Filter by canonical class IRI (for example, <https://atomic.dev/ns/workflow#File>).
    ///
    /// Matches any asserted type exactly and case-sensitively, before pool limits.
    /// Omitted or empty: all types. An unknown class returns no results.
    #[arg(long, short = 't')]
    pub kind: Option<String>,

    /// Candidate retention budget (default: 5000; at least --limit).
    ///
    /// Without --kind, a larger pool allows diversity across types. With --kind,
    /// only matching nodes compete; a small pool cannot hide their top results.
    #[arg(long, short = 'p')]
    pub pool: Option<usize>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryNodes {
    fn run(&self) -> CliResult<()> {
        if crate::commands::rpc::query_search(self)? {
            return Ok(());
        }

        let root = find_repository_root()?;
        let repo = Repository::open_readonly(&root).map_err(CliError::Repository)?;
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        let view = repo
            .desired_view_name(working_copy)
            .map_err(CliError::Repository)?;

        let nodes = self.search_on_view(&repo, &view)?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&nodes).unwrap());
        } else if nodes.is_empty() {
            println!("No results.");
        } else {
            for node in &nodes {
                let summary = node.summary.as_deref().unwrap_or("");
                let summary_display = if summary.is_empty() {
                    String::new()
                } else {
                    format!("  {}", truncate_display(summary, 60))
                };
                println!(
                    "  [{}] {}{}",
                    compact(node.primary_type()),
                    node.id,
                    summary_display
                );
            }
            println!("\n{} result(s).", nodes.len());
        }

        Ok(())
    }
}

impl QueryNodes {
    fn search_on_view(&self, repo: &Repository, view: &str) -> CliResult<Vec<KgNode>> {
        repo.vault_kg_search_on_view_with_kind(
            view,
            &self.query,
            self.limit,
            Some(self.pool.unwrap_or(5000)),
            self.kind.as_deref(),
        )
        .map_err(CliError::Repository)
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use atomic_core::kg::iri;
    use atomic_core::pristine::VaultEntryType;

    #[test]
    fn direct_search_helper_filters_kind_before_pool_limits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let memory = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        repo.vault_store(
            &format!("memory/{memory}.md"),
            VaultEntryType::Memory,
            b":::memory\nFallback regression.\n:::\n".to_vec(),
            serde_json::json!({"id": memory, "kind": "context"}).to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("pool.rs"), format!("// {memory}\n")).unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.add(wc, "pool.rs", Default::default()).unwrap();
        repo.record(
            wc,
            atomic_core::change::ChangeHeader::new("record fallback fixture"),
            atomic_repository::RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
        let memory_id = iri::mint_memory(&repo.project_id().unwrap(), memory);
        for pool in [0, 1, 100] {
            let args = QueryNodes::try_parse_from([
                "search",
                memory,
                "--kind",
                Class::Memory.iri(),
                "--pool",
                &pool.to_string(),
                "--limit",
                "1",
            ])
            .unwrap();
            let nodes = args.search_on_view(&repo, "dev").unwrap();
            assert_eq!(nodes.len(), 1, "pool={pool}");
            assert_eq!(nodes[0].id, memory_id);
            let args = QueryNodes { limit: 0, ..args };
            assert!(args.search_on_view(&repo, "dev").unwrap().is_empty());
        }
        for kind in [None, Some(String::new()), Some("urn:unknown:class".into())] {
            let args = QueryNodes {
                query: memory.into(),
                limit: 1,
                pool: Some(1),
                kind: kind.clone(),
                json: true,
            };
            let nodes = args.search_on_view(&repo, "dev").unwrap();
            if kind.as_deref() == Some("urn:unknown:class") {
                assert!(nodes.is_empty());
            } else {
                assert_eq!(nodes.len(), 1);
                assert!(nodes[0].has_type(Class::File));
            }
            assert!(args.search_on_view(&repo, "missing").is_err());
        }
    }
}

/// Get the neighborhood of a KG node.
#[derive(Parser, Debug)]
pub struct QueryNeighbors {
    /// Canonical resource IRI, copied from query search or graph JSON.
    pub node_id: String,

    /// Traversal depth (1 or 2).
    #[arg(long, short = 'd', default_value = "1")]
    pub depth: u8,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryNeighbors {
    fn run(&self) -> CliResult<()> {
        if crate::commands::rpc::query_neighbors(self)? {
            return Ok(());
        }

        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let subgraph = repo
            .vault_kg_neighbors(&self.node_id, self.depth)
            .map_err(CliError::Repository)?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&subgraph).unwrap());
        } else if subgraph.is_empty() {
            println!("No neighbors found for '{}'.", self.node_id);
        } else {
            println!("Nodes ({}):", subgraph.nodes.len());
            for node in &subgraph.nodes {
                let summary = node.summary.as_deref().unwrap_or("");
                let summary_display = if summary.is_empty() {
                    String::new()
                } else {
                    format!("  {}", truncate_display(summary, 50))
                };
                println!(
                    "  [{}] {}{}",
                    compact(node.primary_type()),
                    node.id,
                    summary_display
                );
            }
            println!("\nEdges ({}):", subgraph.edges.len());
            for edge in &subgraph.edges {
                println!(
                    "  {} \u{2192}[{}]\u{2192} {}",
                    edge.from_id, edge.kind, edge.to_id
                );
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// QueryCallers — find all functions that call a given entity
// ---------------------------------------------------------------------------

/// Find all functions that call a given entity node.
#[derive(Parser, Debug)]
pub struct QueryCallers {
    /// Symbol selector: `<path>#<name>` or a bare symbol name.
    pub entity_id: String,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryCallers {
    fn run(&self) -> CliResult<()> {
        // The service resolves calls through the current view's recorded bytes.
        if crate::commands::rpc::query_callers(self)? {
            return Ok(());
        }
        let root = find_repository_root()?;
        let repo = Repository::open_readonly(&root).map_err(CliError::Repository)?;
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        let view = repo
            .desired_view_name(working_copy)
            .map_err(CliError::Repository)?;

        let callers = repo
            .query_callers_on_view(&view, &self.entity_id)
            .map_err(CliError::Repository)?;
        render_callers(&self.entity_id, &callers, self.json)
    }
}

pub(crate) fn render_callers(
    target: &str,
    callers: &[atomic_repository::repository::Caller],
    json: bool,
) -> CliResult<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(callers).map_err(|e| CliError::Internal(e.into()))?
        );
    } else if callers.is_empty() {
        println!("No callers found for '{target}'.");
    } else {
        println!("{} caller(s) of {target}:\n", callers.len());
        for caller in callers {
            println!(
                "  {}#{}:{}{}",
                caller.path,
                caller.name,
                caller.line,
                caller
                    .summary
                    .as_ref()
                    .map(|summary| format!("  [{summary}]"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// QueryEntities — list tree-sitter entities in a file
// ---------------------------------------------------------------------------

/// List tree-sitter entities in a file.
#[derive(Parser, Debug)]
pub struct QueryEntities {
    /// File path (relative to repo root).
    pub path: String,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryEntities {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;

        if !atomic_semantic::is_supported(&self.path) {
            if self.json {
                return Err(CliError::InvalidArgument {
                    message: format!("Unsupported file type: {}", self.path),
                });
            }
            println!("Unsupported file type: {}", self.path);
            return Ok(());
        }

        let full_path = root.join(&self.path);
        let source = std::fs::read_to_string(&full_path).map_err(CliError::Io)?;

        let mut registry = atomic_semantic::ParserRegistry::new();
        let entities = registry.extract(&self.path, &source);

        if self.json {
            println!("{}", serde_json::to_string_pretty(&entities).unwrap());
        } else if entities.is_empty() {
            println!("No entities found in {}", self.path);
        } else {
            for e in &entities {
                let exported = if e.exported { "  [exported]" } else { "" };
                let sig = e
                    .signature
                    .as_deref()
                    .map(|s| format!("  {}", s))
                    .unwrap_or_default();
                println!(
                    "  {:<12} {:<30} L{}-{}{}{}",
                    e.kind.as_str(),
                    e.name,
                    e.line,
                    e.end_line,
                    exported,
                    sig,
                );
            }
            println!("\n{} entity(ies) in {}", entities.len(), self.path);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// QueryCode — search source code content via the syntext index
// ---------------------------------------------------------------------------

/// Search recorded source code in the current view.
#[derive(Parser, Debug)]
pub struct QueryCode {
    /// Search pattern (regex supported).
    pub pattern: String,

    /// Restrict to files matching this path pattern.
    #[arg(long, short = 'g')]
    pub path_filter: Option<String>,

    /// Restrict to this file type (e.g., "rs", "cpp", "py").
    #[arg(long, short = 't')]
    pub file_type: Option<String>,

    /// Maximum results to return.
    #[arg(long, short = 'n', default_value = "30")]
    pub max_results: usize,

    /// Case-insensitive search.
    #[arg(long, short = 'i')]
    pub case_insensitive: bool,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryCode {
    fn run(&self) -> CliResult<()> {
        if crate::commands::rpc::query_code(self)? {
            return Ok(());
        }
        let root = find_repository_root()?;
        let repo = Repository::open_readonly(&root).map_err(CliError::Repository)?;
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        let view = repo
            .desired_view_name(working_copy)
            .map_err(CliError::Repository)?;

        let opts = atomic_repository::ContentSearchOptions {
            path_filter: self.path_filter.clone(),
            file_type: self.file_type.clone(),
            exclude_type: None,
            max_results: Some(self.max_results),
            case_insensitive: self.case_insensitive,
        };

        let result = repo
            .search_content_on_view(&view, &self.pattern, opts)
            .map_err(CliError::Repository)?;

        print_code_result(&result, self.json)
    }
}

pub(crate) fn print_code_result(
    result: &atomic_repository::ContentSearchResult,
    json: bool,
) -> CliResult<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else if result.matches.is_empty() {
        println!("No matches found.");
    } else {
        // Show matches
        let mut files = std::collections::HashSet::new();
        for m in &result.matches {
            files.insert(m.path.as_str());
            println!(
                "{}:{}: {}",
                m.path,
                m.line_number,
                m.line_content.trim_end()
            );
        }

        // Summary
        println!(
            "\n{} match(es) in {} file(s).",
            result.matches.len(),
            files.len()
        );

        // Show total and facets when results were truncated
        if result.total_matches > result.matches.len() {
            println!(
                "  ({} total matches — showing top {}. Narrow with -g or -t.)\n",
                result.total_matches,
                result.matches.len()
            );
            println!("  Top directories:");
            for (dir, count) in &result.dir_facets {
                println!("    {:<40} {} matches", format!("-g \"{}\"", dir), count);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// QueryIndex — build / rebuild the content search index
// ---------------------------------------------------------------------------

/// Build or update the content search index.
#[derive(Parser, Debug)]
pub struct QueryIndex {
    /// Full rebuild instead of incremental update.
    #[arg(long)]
    pub rebuild: bool,

    /// Show index statistics after building.
    #[arg(long)]
    pub stats: bool,
}

impl Command for QueryIndex {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;

        if self.rebuild || !atomic_repository::has_content_index(&root) {
            print!("Building content index...");
            atomic_repository::build_content_index(&root)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("index build failed: {e}")))?;
            println!(" done.");
        } else {
            print!("Updating content index...");
            atomic_repository::update_content_index(&root)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("index update failed: {e}")))?;
            println!(" done.");
        }

        if self.stats {
            if let Some(stats) = atomic_repository::content_index_stats(&root) {
                println!(
                    "  files: {}, segments: {}, index size: {:.1} MB",
                    stats.total_documents,
                    stats.total_segments,
                    stats.index_size_bytes as f64 / (1024.0 * 1024.0),
                );
            }
        }

        Ok(())
    }
}

/// Enrich the KG from VCS data (changes, files, views, dependencies).
#[derive(Parser, Debug)]
pub struct QueryEnrich {
    /// Enrich only this exact recorded change. Repeat for multiple changes.
    /// Without this option, all VCS-derived KG data is rebuilt.
    #[arg(long = "change", value_name = "FULL_HASH")]
    pub changes: Vec<String>,

    /// Force a clean rebuild: drop all previously enriched VCS/AST-derived
    /// KG nodes (files, modules, views, changes, entities) before
    /// re-deriving them.
    ///
    /// A plain enrich only upserts, so nodes that no longer apply — files
    /// that became ignored or were deleted — linger as stale entries. Pass
    /// `--rebuild` to reconcile. Vault and session nodes are never affected.
    /// Cannot be combined with `--change`.
    #[arg(long, conflicts_with = "changes")]
    pub rebuild: bool,
}

fn parse_change_hash(value: &str) -> CliResult<Hash> {
    Hash::from_base32(value.as_bytes()).ok_or_else(|| CliError::InvalidArgument {
        message: format!("invalid full change hash '{value}'; expected a 52-character base32 hash"),
    })
}

#[cfg(test)]
mod enrich_tests {
    use super::*;
    use atomic_core::change::ChangeHeader;
    use atomic_repository::record::RecordOptions;
    use std::path::PathBuf;

    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap(),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    fn parses_repeated_targeted_change_flags() {
        let first = "A".repeat(52);
        let second = "B".repeat(52);
        let parsed = QueryEnrich::try_parse_from([
            "enrich",
            "--change",
            first.as_str(),
            "--change",
            second.as_str(),
        ])
        .unwrap();

        assert_eq!(parsed.changes, vec![first, second]);
    }

    #[test]
    fn rejects_change_prefix_for_targeted_enrichment() {
        let error = parse_change_hash("ABC123").unwrap_err();
        assert!(matches!(error, CliError::InvalidArgument { .. }));
        assert!(error.to_string().contains("expected a 52-character"));
    }

    #[test]
    #[serial_test::file_serial]
    fn targeted_enrichment_materializes_a_change_recorded_without_kg_work() {
        let _guard = DirGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        repo.init_kg().unwrap();

        std::fs::write(temp.path().join("lesson.txt"), "durable fact\n").unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.add(working_copy, "lesson.txt", Default::default())
            .unwrap();
        let outcome = repo
            .record(
                working_copy,
                ChangeHeader::builder()
                    .message("Agent-authored change")
                    .build(),
                RecordOptions::new().enrich_kg(false),
            )
            .unwrap();
        let full_hash = outcome.hash().to_base32();
        let node_id = atomic_core::kg::iri::mint_change(outcome.hash());
        assert!(repo.vault_kg_node(&node_id).unwrap().is_none());
        drop(repo);

        std::env::set_current_dir(temp.path()).unwrap();
        QueryEnrich {
            changes: vec![full_hash],
            rebuild: false,
        }
        .run()
        .unwrap();

        let repo = Repository::open(temp.path()).unwrap();
        assert!(repo.vault_kg_node(&node_id).unwrap().is_some());
    }
}

impl Command for QueryEnrich {
    fn run(&self) -> CliResult<()> {
        if crate::commands::rpc::query_enrich(self)? {
            return Ok(());
        }

        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;

        // Agent stop hooks intentionally record with KG enrichment disabled to
        // keep their latency low. Consumers that need immediate RDF lineage can
        // enrich only the exact changes they are about to reference instead of
        // scanning every view, file, module, change, and source entity.
        if !self.changes.is_empty() {
            let hashes = self
                .changes
                .iter()
                .map(|value| parse_change_hash(value))
                .collect::<CliResult<Vec<_>>>()?;

            for hash in &hashes {
                repo.enrich_change(working_copy, hash)
                    .map_err(CliError::Repository)?;
            }

            println!("Enriched {} change(s).", hashes.len());
            return Ok(());
        }

        // Shared file/view resources also carry vault/session relationships;
        // regenerate the whole read model rather than cascade-delete those joins.
        if self.rebuild {
            repo.rebuild_kg(true).map_err(CliError::Repository)?;
        }

        let mut total = atomic_repository::KgEnrichStats::default();

        // Phase 1: Views
        let spinner = create_spinner("Enriching views...");
        match repo.kg_enrich_views() {
            Ok(n) => {
                total.views = n;
                finish_success(&spinner, &format!("{} view(s)", n));
            }
            Err(e) => finish_error(&spinner, &format!("views failed: {}", e)),
        }

        // Phase 2: Files
        let spinner = create_spinner("Enriching files...");
        match repo.kg_enrich_files(working_copy) {
            Ok(n) => {
                total.files = n;
                finish_success(&spinner, &format!("{} file(s)", n));
            }
            Err(e) => finish_error(&spinner, &format!("files failed: {}", e)),
        }

        // Phase 4: Changes
        let spinner = create_spinner("Enriching changes...");
        match repo.kg_enrich_changes(working_copy) {
            Ok(n) => {
                total.changes = n;
                finish_success(&spinner, &format!("{} change(s)", n));
            }
            Err(e) => finish_error(&spinner, &format!("changes failed: {}", e)),
        }

        // Phase 8: Content search index (syntext)
        let spinner = create_spinner("Building content index...");
        match atomic_repository::build_content_index(&root) {
            Ok(()) => {
                if let Some(stats) = atomic_repository::content_index_stats(&root) {
                    finish_success(
                        &spinner,
                        &format!(
                            "{} file(s) indexed ({:.1} MB)",
                            stats.total_documents,
                            stats.index_size_bytes as f64 / (1024.0 * 1024.0),
                        ),
                    );
                } else {
                    finish_success(&spinner, "done");
                }
            }
            Err(e) => finish_error(&spinner, &format!("content index failed: {}", e)),
        }

        println!("\nEnriched: {}", total);

        Ok(())
    }
}

/// Rebuild the KG index from all vault entries.
#[derive(Parser, Debug)]
pub struct Reindex {
    /// Rebuild the complete workflow graph from canonical objects.
    #[arg(long)]
    pub full: bool,
}

impl Command for Reindex {
    fn run(&self) -> CliResult<()> {
        crate::commands::rpc::query_reindex(self.full)
    }
}

/// Rebuild embeddings for vault content.
#[derive(Parser, Debug)]
pub struct QueryEmbed {
    /// Embed only this specific path.
    #[arg(long, short = 'p')]
    pub path: Option<String>,
}

impl Command for QueryEmbed {
    fn run(&self) -> CliResult<()> {
        // Route every form through the service layer: the provider
        // resolves handler-side from the same environment/config; the CLI
        // prints the local report from the wire-carried outcome.
        if crate::commands::rpc::query_embed(self)? {
            return Ok(());
        }
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let provider = atomic_repository::resolve_embedding_provider();
        let dims = provider.dimensions;
        let config = atomic_repository::EmbedConfig {
            max_chunk_tokens: 512,
            dimensions: dims,
        };

        // Use the resolved provider's sync embedding function
        let embed_fn = |text: &str| -> Vec<f32> {
            provider
                .embed_sync(&[text.to_string()])
                .ok()
                .and_then(|v| v.into_iter().next())
                .unwrap_or_else(|| atomic_repository::hash_embed(text, dims))
        };

        println!("Using embedding provider: {} ({}d)", provider.model, dims);

        if let Some(ref path) = self.path {
            let count = repo
                .vault_embed(path, &embed_fn, &config)
                .map_err(CliError::Repository)?;
            if count == 0 {
                println!("Content unchanged, skipped: {}", path);
            } else {
                println!("Embedded {} chunks: {}", count, path);
            }
        } else {
            let count = repo
                .vault_embed_all(&embed_fn, &config)
                .map_err(CliError::Repository)?;
            println!("Embedded {} total chunks.", count);
        }

        Ok(())
    }
}

/// Execute a structured query plan against the knowledge graph.
///
/// Reads a JSON query plan from stdin and executes it.
/// Use `--json` for machine-readable output.
///
/// Example plan:
/// {"schema_version":1,"steps":[{"type":"kg_search","query":"auth","limit":5,"bind":"results"}]}
#[derive(Parser, Debug)]
pub struct PlanExec {
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for PlanExec {
    fn run(&self) -> CliResult<()> {
        // Route every form through the service layer (stdin stays
        // client-side; the plan executes handler-side against the repo).
        if crate::commands::rpc::query_plan(self)? {
            return Ok(());
        }
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Read plan from stdin
        use std::io::Read;
        let mut plan_json = String::new();
        std::io::stdin()
            .read_to_string(&mut plan_json)
            .map_err(CliError::Io)?;

        let plan = atomic_repository::parse_plan(&plan_json).map_err(CliError::Repository)?;

        let result = atomic_repository::execute_plan(&repo, &plan).map_err(CliError::Repository)?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
        } else {
            // Human-readable output
            println!(
                "Plan executed in {}ms ({} steps)\n",
                result.elapsed_ms,
                result.step_stats.len()
            );

            for (i, stat) in result.step_stats.iter().enumerate() {
                println!(
                    "  Step {}: {} → {} results ({}ms)",
                    i + 1,
                    stat.step_type,
                    stat.result_count,
                    stat.elapsed_ms
                );
            }

            if !result.nodes.is_empty() {
                println!("\nNodes ({}):", result.nodes.len());
                for node in &result.nodes {
                    print!("  [{}] {}", compact(node.primary_type()), node.label);
                    if let Some(ref s) = node.summary {
                        // Truncate safely for Unicode text.
                        let short: String = s.chars().take(60).collect();
                        print!(": {}", short);
                    }
                    println!();
                }
            }

            if !result.edges.is_empty() {
                println!("\nEdges ({}):", result.edges.len());
                for edge in result.edges.iter().take(20) {
                    println!("  {} -[{}]-> {}", edge.from_id, edge.kind, edge.to_id);
                }
                if result.edges.len() > 20 {
                    println!("  ... and {} more", result.edges.len() - 20);
                }
            }

            if !result.content.is_empty() {
                println!("\nContent ({} entries):", result.content.len());
                for (path, text) in &result.content {
                    // Truncate safely for Unicode text.
                    let preview: String = text.chars().take(100).collect();
                    println!("  {}: {}...", path, preview.replace('\n', " "));
                }
            }
        }

        Ok(())
    }
}

/// Ask a question — searches the KG and generates an LLM answer.
///
/// Requires `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` to be set.
/// Without an API key, falls back to search-only mode.
///
/// # Architecture
///
/// 1. **Tokenize** the question → extract meaningful search terms
/// 2. **KG search** per term → collect results, bucket into `HashMap<kind, Vec<Node>>`
/// 3. **Grep source files** for the search terms with surrounding context lines
/// 4. **Build structured prompt** with KG structure + grep'd source context
/// 5. **Send to LLM** for synthesis
#[derive(Parser, Debug)]
pub struct QueryAsk {
    /// Natural language question.
    pub question: String,

    /// Maximum agentic tool-use turns (default 5).
    #[arg(long, short = 't', default_value = "5")]
    pub max_turns: u8,

    /// Output as JSON (includes tool trace).
    #[arg(long)]
    pub json: bool,

    /// Show live tool calls as they execute.
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

/// System prompt for the agentic query loop.
const ASK_SYSTEM_PROMPT: &str = "\
You are a code assistant for a repository tracked by Atomic VCS (not git).

You have tools to explore the repository:
- kg_search: search the knowledge graph (files, functions, changes, views, goals, intents)
- kg_neighbors: explore connections around a node
- read_file: read source files (with optional line ranges)
- code_search: search source code content for a pattern (regex, path/type filters)
- list_entities: list functions/classes/types in a file via tree-sitter
- vault_read: read vault entries (goals, intents, memories)

Strategy:
1. Search first: use kg_search or code_search to find relevant files and line numbers.
2. Use list_entities to see what functions/classes a file contains and their line ranges.
3. Use kg_neighbors to follow relationships (which changes modified a file, what it depends on).
4. NEVER call read_file without start_line and end_line. Always use list_entities or \
   code_search first to find the exact line range you need, then read just that range.
5. Answer concisely using what you found. Cite file paths and line numbers.

The knowledge graph contains:
- nodes with asserted class IRIs in https://atomic.dev/ns/workflow# (File, Change, View, WorkIntent, Goal, Memory)
- project-qualified resource IRIs and global full-hash change IRIs; copy IDs from search results
- predicates: atomic:modifiesFile, atomic:authoredBy, atomic:directlyDependsOnChange, atomic:references, atomic:directlyOnView, atomic:hasPlan, atomic:implements
- views filter membership in one common graph; directlyOnView is own membership, not inherited visibility
- source entities and callers are parsed live with list_entities/code_search, never stored in the workflow graph.";

impl Command for QueryAsk {
    fn run(&self) -> CliResult<()> {
        // Route every form through the service layer: the agentic tool
        // loop runs handler-side over the repository. --verbose's live
        // tool-call trace is the unary-RPC limit: the trace rides the
        // result (rendered in --json), not streamed live.
        if crate::commands::rpc::query_ask(self)? {
            return Ok(());
        }
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let llm = atomic_repository::resolve_llm_provider().ok_or_else(|| {
            CliError::Internal(anyhow::anyhow!(
                "No API key configured. Set ANTHROPIC_API_KEY or OPENAI_API_KEY."
            ))
        })?;

        let executor = atomic_repository::RepoToolExecutor::new(&repo);
        let config = atomic_repository::AgentConfig {
            system_prompt: ASK_SYSTEM_PROMPT.to_string(),
            max_turns: self.max_turns,
            max_tokens: 4096,
            verbose: self.verbose,
        };

        let start = std::time::Instant::now();
        let result =
            atomic_repository::run_tool_loop_sync(&llm, &executor, &self.question, &config)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("LLM error: {e}")))?;
        let elapsed = start.elapsed();

        if self.json {
            let json = serde_json::json!({
                "query": self.question,
                "answer": result.answer,
                "model": result.model,
                "turns": result.turns,
                "input_tokens": result.total_usage.input_tokens,
                "output_tokens": result.total_usage.output_tokens,
                "tool_trace": result.tool_trace,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            println!("{}\n", result.answer);
            let cache_info = if result.total_usage.cache_read_tokens > 0 {
                format!(
                    ", {} cache read + {} cache write",
                    result.total_usage.cache_read_tokens, result.total_usage.cache_creation_tokens
                )
            } else {
                String::new()
            };
            let secs = elapsed.as_secs_f64();
            let duration = if secs < 60.0 {
                format!("{:.1}s", secs)
            } else {
                format!("{}m {:.0}s", (secs / 60.0) as u64, secs % 60.0)
            };
            println!(
                "  — {} ({} turns, {}, {} in + {} out tokens{})",
                result.model,
                result.turns,
                duration,
                result.total_usage.input_tokens,
                result.total_usage.output_tokens,
                cache_info,
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_display;

    #[test]
    fn html_palette_covers_every_workflow_class_with_unique_high_contrast_colors() {
        use atomic_core::kg::ontology::Class;
        fn luminance(hex: &str) -> f64 {
            let channel = |offset| {
                let value =
                    f64::from(u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap()) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
        }
        let colors: std::collections::HashSet<_> = Class::ALL
            .iter()
            .map(|class| super::html_node_color(class.iri()))
            .collect();
        assert_eq!(
            colors.len(),
            Class::ALL.len(),
            "known types must not share colors"
        );
        let background = luminance("#1a1a2e");
        for color in colors {
            assert!(color.starts_with('#') && color.len() == 7);
            assert!(
                (luminance(&color) + 0.05) / (background + 0.05) >= 4.5,
                "low contrast: {color}"
            );
        }
    }

    #[test]
    fn html_external_type_colors_are_stable_and_not_the_old_gray_fallback() {
        let first = super::html_node_color("https://example.test/TypeA");
        assert_eq!(first, super::html_node_color("https://example.test/TypeA"));
        assert!(first.starts_with("hsl("));
        assert_ne!(first, super::html_node_color("https://example.test/TypeB"));
    }

    #[test]
    fn html_inspector_embeds_full_snapshot_data_without_script_injection() {
        use atomic_core::{
            kg::ontology::{Class, Predicate},
            KgEdge, KgNode,
        };
        let hostile = "</script><script>alert('metadata')</script>";
        let mut node = KgNode::new("urn:node:first", Class::Change, hostile, "vcs");
        node.summary = Some("full summary ".repeat(40));
        node.metadata =
            Some(serde_json::json!({"hash":"FULL_HASH", "nested":{"evidence":hostile}}));
        let neighbor = KgNode::new("urn:node:second", Class::File, "src/cli.ts", "vcs");
        let mut edge = KgEdge::new(&node.id, &neighbor.id, Predicate::ModifiesFile);
        edge.metadata = Some(serde_json::json!({"evidence":hostile}));
        let html = super::emit_html(
            "Inspect <metadata>",
            &[node.clone(), neighbor],
            &[edge.clone()],
        );
        let json = html
            .split("var graphData = ")
            .nth(1)
            .unwrap()
            .split(";\n")
            .next()
            .unwrap();
        let data: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(data["nodes"][0]["metadata"], node.metadata.unwrap());
        assert_eq!(data["nodes"][0]["summary"], node.summary.unwrap());
        assert_eq!(data["nodes"][0]["source"], "vcs");
        assert_eq!(
            data["nodes"][0]["color"],
            super::html_node_color(Class::Change.iri())
        );
        assert_eq!(data["links"][0]["metadata"], edge.metadata.unwrap());
        assert!(!html.contains(hostile));
        assert!(html.contains("\\u003c/script\\u003e"));
        assert!(html.contains("aria-label=\"Node details\""));
        assert!(html.contains("if (dragMoved) return;"));
        assert!(html.contains("graphView.clientWidth"));
    }

    #[test]
    fn test_truncate_display_short_ascii_unchanged() {
        assert_eq!(truncate_display("hello", 60), "hello");
    }

    #[test]
    fn test_truncate_display_long_ascii_truncated() {
        let s = "a".repeat(80);
        let out = truncate_display(&s, 60);
        assert_eq!(out, format!("{}...", "a".repeat(57)));
    }

    #[test]
    fn test_truncate_display_exact_limit_unchanged() {
        let s = "b".repeat(60);
        assert_eq!(truncate_display(&s, 60), s);
    }

    #[test]
    fn test_truncate_display_three_byte_chars_no_panic() {
        // Regression test for three-byte characters.
        let s = format!("x{}", "\u{20ac}".repeat(40));
        assert!(s.len() > 60); // byte length exceeds the old byte-based check
        assert_eq!(truncate_display(&s, 60), s); // 41 chars: no truncation
    }

    #[test]
    fn test_truncate_display_three_byte_chars_truncated() {
        let s = format!("x{}", "\u{20ac}".repeat(80)); // 81 chars, 241 bytes
        let out = truncate_display(&s, 60);
        assert_eq!(out, format!("x{}...", "\u{20ac}".repeat(56)));
    }

    #[test]
    fn test_truncate_display_four_byte_chars_within_limit_unchanged() {
        // Regression test for four-byte characters.
        let s = "\u{1f680}".repeat(40);
        assert_eq!(truncate_display(&s, 50), s);
    }

    #[test]
    fn test_truncate_display_four_byte_chars_truncated() {
        let s = "\u{1f680}".repeat(80);
        let out = truncate_display(&s, 50);
        assert_eq!(out, format!("{}...", "\u{1f680}".repeat(47)));
    }
}
