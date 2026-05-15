//! `atomic vault query` — query the vault knowledge graph.

use std::collections::{HashMap, HashSet};

use clap::{Parser, Subcommand};

use atomic_core::pristine::vault::{KgEdge, KgNode};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{create_spinner, finish_error, finish_success};

/// Subcommands for vault knowledge graph queries.
#[derive(Subcommand, Debug)]
pub enum QueryCommands {
    /// Search the knowledge graph by keywords.
    ///
    /// Performs full-text search over KG node labels and summaries,
    /// returning matching nodes ranked by relevance.
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
    /// The index is stored at `.atomic/content-index/` and is required by the
    /// `code` subcommand.
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

    /// Search source code content using the syntext index.
    ///
    /// Searches the indexed content of all files in the repository.
    /// Requires the content index to be built first via `atomic vault query enrich`.
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

    /// Rebuild the KG from VCS data (changes, files, views).
    ///
    /// VCS data is enriched into the KG automatically after each record.
    /// Use this command to do a full rebuild.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query enrich
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
            QueryCommands::Search(cmd) => cmd.run(),
            QueryCommands::Neighbors(cmd) => cmd.run(),
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
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let kind_allowed =
            |kind: &str| -> bool { allow_all || allowed_kinds.contains(&kind.to_lowercase()) };

        // 1. Seed: search the KG, filtered to allowed kinds
        let seed_nodes: Vec<KgNode> = repo
            .vault_kg_search(&self.query, self.limit * 3, None) // fetch extra, then filter
            .map_err(CliError::Repository)?
            .into_iter()
            .filter(|n| kind_allowed(&n.kind))
            .take(self.limit)
            .collect();

        if seed_nodes.is_empty() {
            if self.json {
                println!("{{\"nodes\":[],\"edges\":[]}}");
            } else {
                eprintln!("No results for {:?} (kinds: {}).", self.query, self.kinds);
            }
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
                if !kind_allowed(&node.kind) {
                    continue;
                }
                // Cap change nodes per seed
                if node.kind == "change" && cap_changes {
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
                let from_kind_ok = allow_all || {
                    let prefix = edge.from_id.split(':').next().unwrap_or("");
                    kind_allowed(prefix)
                };
                let to_kind_ok = allow_all || {
                    let prefix = edge.to_id.split(':').next().unwrap_or("");
                    kind_allowed(prefix)
                };
                if from_kind_ok && to_kind_ok {
                    // Only add edge if both endpoints made it into node_map
                    // (respects the per-seed change cap)
                    if node_map.contains_key(&edge.from_id) || node_map.contains_key(&edge.to_id) {
                        let key = (edge.from_id.clone(), edge.to_id.clone(), edge.kind.clone());
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
fn emit_dot(nodes: &[KgNode], edges: &[KgEdge]) -> String {
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
        let (fillcolor, shape) = node_style(&node.kind);
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
            dot_escape_label(&edge.kind),
        ));
    }

    out.push_str("}\n");
    out
}

/// Returns `(fillcolor, shape)` for a given node kind.
fn node_style(kind: &str) -> (&'static str, &'static str) {
    match kind.to_lowercase().as_str() {
        "module" => ("#d4e6f1", "folder"),
        "file" => ("#d5f5e3", "box"),
        "entity" => ("#fdebd0", "ellipse"),
        "change" => ("#fadbd8", "note"),
        "view" => ("#e8daef", "pentagon"),
        "identity" => ("#fef9e7", "box"),
        "goal" => ("#d1f2eb", "hexagon"),
        "intent" => ("#d1f2eb", "hexagon"),
        "memory" => ("#fef9e7", "box"),
        _ => ("#f2f3f4", "box"),
    }
}

/// Build a human-readable label for a node in DOT output.
fn node_label(node: &KgNode) -> String {
    let kind = node.kind.to_lowercase();
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
        "file" => {
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
        "change" => {
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

/// Generate a self-contained HTML page with a D3.js force-directed graph.
fn emit_html(query: &str, nodes: &[KgNode], edges: &[KgEdge]) -> String {
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
                "kind": n.kind,
                "label": n.label,
                "summary": n.summary.as_deref().unwrap_or(""),
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
            })
        })
        .collect();

    let graph_data = serde_json::json!({
        "nodes": graph_nodes,
        "links": graph_edges,
    });

    let data_json = serde_json::to_string(&graph_data).unwrap();
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
  canvas {{ display: block; width: 100vw; height: 100vh; }}
  #tooltip {{ position: fixed; padding: 10px 14px; background: rgba(0,0,0,0.92); border: 1px solid #555; border-radius: 6px; font-size: 12px; pointer-events: none; display: none; z-index: 20; max-width: 420px; line-height: 1.5; }}
  #tooltip .kind {{ color: #888; font-size: 10px; text-transform: uppercase; letter-spacing: 0.5px; }}
  #tooltip .label {{ color: #fff; font-weight: 600; margin: 3px 0; word-break: break-all; }}
  #tooltip .summary {{ color: #bbb; font-size: 11px; margin-bottom: 2px; }}
  #tooltip .detail {{ color: #aaa; font-size: 11px; }}
  #legend {{ position: fixed; bottom: 16px; right: 16px; z-index: 10; background: rgba(26,26,46,0.92); border: 1px solid #333; border-radius: 8px; padding: 10px 14px; font-size: 11px; }}
  #legend .legend-title {{ color: #a0a0c0; font-weight: 600; font-size: 10px; text-transform: uppercase; letter-spacing: 0.5px; margin-bottom: 6px; }}
  #legend .legend-item {{ display: flex; align-items: center; gap: 8px; margin: 3px 0; color: #ccc; }}
  #legend .legend-dot {{ width: 10px; height: 10px; border-radius: 50%; flex-shrink: 0; }}
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
<canvas id="graph"></canvas>
<div id="legend"></div>
<script src="https://cdn.jsdelivr.net/npm/d3@7"></script>
<script>
(function() {{
  // Embedded graph data
  var graphData = {data_json};

  var COLORS = {{
    module:   '#3498db',
    file:     '#2ecc71',
    entity:   '#e67e22',
    change:   '#e74c3c',
    view:     '#9b59b6',
    identity: '#f1c40f',
    goal:     '#1abc9c',
    intent:   '#1abc9c',
    memory:   '#f39c12',
  }};
  var EDGE_COLORS = {{
    DEFINES:     '#e67e22',
    MODIFIES:    '#e74c3c',
    PART_OF:     '#3498db',
    INCLUDES:    '#2ecc71',
    AUTHORED_BY: '#f1c40f',
    DEPENDS_ON:  '#e74c3c',
    ON_VIEW:     '#9b59b6',
    REFERENCES:  '#95a5a6',
  }};
  var DEFAULT_COLOR = '#7f8c8d';

  function nodeRadius(d) {{
    if (d.kind === 'module') return 14 + Math.min(d.content_matches, 50) * 0.3;
    if (d.kind === 'file') return 5 + Math.min(d.content_matches, 100) * 0.12;
    if (d.kind === 'entity') return 4;
    if (d.kind === 'change') return 4;
    return 5;
  }}

  function nodeColor(d) {{ return COLORS[d.kind] || DEFAULT_COLOR; }}
  function edgeColor(d) {{ return EDGE_COLORS[d.kind] || '#555'; }}

  // Canvas setup
  var canvas = document.getElementById('graph');
  var ctx = canvas.getContext('2d');
  var dpr = window.devicePixelRatio || 1;
  var width = window.innerWidth;
  var height = window.innerHeight;
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

  var links = graphData.links.filter(function(l) {{
    return nodeById.has(l.source) && nodeById.has(l.target);
  }});

  document.getElementById('stats').textContent = nodes.length + ' nodes \u00b7 ' + links.length + ' edges';

  // Build legend from kinds present in data
  var kindsPresent = new Set(nodes.map(function(n) {{ return n.kind; }}));
  var legendEl = document.getElementById('legend');
  var legendHTML = '<div class="legend-title">Node Types</div>';
  var kindOrder = ['module', 'file', 'entity', 'change', 'view', 'identity', 'goal', 'intent', 'memory'];
  kindOrder.forEach(function(k) {{
    if (kindsPresent.has(k)) {{
      legendHTML += '<div class="legend-item"><span class="legend-dot" style="background:' + (COLORS[k] || DEFAULT_COLOR) + '"></span>' + k + '</div>';
    }}
  }});
  legendEl.innerHTML = legendHTML;

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
    }});

    // Draw labels for large nodes (modules and high-match files)
    ctx.globalAlpha = 0.85;
    ctx.fillStyle = '#ccc';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'top';
    nodes.forEach(function(d) {{
      if (d.r > 10) {{
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

  canvas.addEventListener('mousemove', function(e) {{
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

  // Handle resize
  window.addEventListener('resize', function() {{
    width = window.innerWidth;
    height = window.innerHeight;
    canvas.width = width * dpr;
    canvas.height = height * dpr;
    canvas.style.width = width + 'px';
    canvas.style.height = height + 'px';
    ctx.scale(dpr, dpr);
    simulation.force('center', d3.forceCenter(width / 2, height / 2));
    simulation.alpha(0.1).restart();
  }});
}})();
</script>
</body>
</html>"##,
        query = escaped_query,
        data_json = data_json,
    )
}

/// Search the knowledge graph.
#[derive(Parser, Debug)]
pub struct QueryNodes {
    /// Search query text (keyword search over node labels and summaries).
    pub query: String,

    /// Maximum results.
    #[arg(long, short = 'k', default_value = "10")]
    pub limit: usize,

    /// Filter by node kind (change, entity, file, view).
    ///
    /// When set, only nodes of this kind are returned.
    /// Without this flag, all kinds are returned.
    #[arg(long, short = 't')]
    pub kind: Option<String>,

    /// Candidate pool size.
    ///
    /// How many candidates to score before selecting the top results.
    /// A larger pool surfaces lower-scored node kinds (e.g. changes)
    /// that would otherwise be crowded out by files with content matches.
    /// Default: same as --limit.
    #[arg(long, short = 'p')]
    pub pool: Option<usize>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryNodes {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // When filtering by kind, we need enough candidates of that kind
        // to survive the diversity selection.  Ask for a large pool so the
        // heap retains many candidates, then post-filter to the target kind.
        let (fetch_limit, pool) = if self.kind.is_some() {
            let pool_val = self.pool.unwrap_or(5000);
            // fetch_limit = pool so all pool candidates become results
            (pool_val, Some(pool_val))
        } else {
            (self.limit, self.pool)
        };

        let all_nodes = repo
            .vault_kg_search(&self.query, fetch_limit, pool)
            .map_err(CliError::Repository)?;

        let nodes: Vec<_> = if let Some(ref kind_filter) = self.kind {
            let k = kind_filter.to_lowercase();
            all_nodes
                .into_iter()
                .filter(|n| n.kind.to_lowercase() == k)
                .take(self.limit)
                .collect()
        } else {
            all_nodes.into_iter().take(self.limit).collect()
        };

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
                    let truncated = if summary.len() > 60 {
                        format!("{}...", &summary[..57])
                    } else {
                        summary.to_string()
                    };
                    format!("  {}", truncated)
                };
                println!("  [{}] {}{}", node.kind, node.id, summary_display);
            }
            println!("\n{} result(s).", nodes.len());
        }

        Ok(())
    }
}

/// Get the neighborhood of a KG node.
#[derive(Parser, Debug)]
pub struct QueryNeighbors {
    /// Node ID (e.g., "change:abc123", "file:src/auth.rs", "intent:PIMO-1").
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
                    let truncated = if summary.len() > 50 {
                        format!("{}...", &summary[..47])
                    } else {
                        summary.to_string()
                    };
                    format!("  {}", truncated)
                };
                println!("  [{}] {}{}", node.kind, node.id, summary_display);
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

#[cfg(feature = "ast")]
impl Command for QueryEntities {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let _repo = Repository::open(&root).map_err(CliError::Repository)?;

        if !atomic_semantic::is_supported(&self.path) {
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

#[cfg(not(feature = "ast"))]
impl Command for QueryEntities {
    fn run(&self) -> CliResult<()> {
        println!("Tree-sitter support not enabled. Rebuild with --features ast.");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// QueryCode — search source code content via the syntext index
// ---------------------------------------------------------------------------

/// Search source code content.
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
        let root = find_repository_root()?;

        let opts = atomic_repository::ContentSearchOptions {
            path_filter: self.path_filter.clone(),
            file_type: self.file_type.clone(),
            exclude_type: None,
            max_results: Some(self.max_results),
            case_insensitive: self.case_insensitive,
        };

        let result = match atomic_repository::search_content(&root, &self.pattern, opts) {
            Ok(r) => r,
            Err(atomic_repository::ContentSearchError::IndexNotFound) => {
                println!(
                    "Content index not found.\n\n  \
                         Run `atomic vault query enrich` to build it, then retry."
                );
                return Ok(());
            }
            Err(e) => {
                return Err(CliError::Internal(anyhow::anyhow!(
                    "code search failed: {e}"
                )));
            }
        };

        if self.json {
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

/// Rebuild the KG from VCS data (changes, files, views, dependencies).
#[derive(Parser, Debug)]
pub struct QueryEnrich;

impl Command for QueryEnrich {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

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
        match repo.kg_enrich_files() {
            Ok(n) => {
                total.files = n;
                finish_success(&spinner, &format!("{} file(s)", n));
            }
            Err(e) => finish_error(&spinner, &format!("files failed: {}", e)),
        }

        // Phase 3: Modules + PART_OF edges
        let spinner = create_spinner("Enriching modules...");
        match repo.kg_enrich_modules() {
            Ok(n) => {
                total.modules = n;
                finish_success(&spinner, &format!("{} module(s)", n));
            }
            Err(e) => finish_error(&spinner, &format!("modules failed: {}", e)),
        }

        // Phase 4: Changes
        let spinner = create_spinner("Enriching changes...");
        match repo.kg_enrich_changes() {
            Ok(n) => {
                total.changes = n;
                finish_success(&spinner, &format!("{} change(s)", n));
            }
            Err(e) => finish_error(&spinner, &format!("changes failed: {}", e)),
        }

        // Phase 5: AST entities
        #[cfg(feature = "ast")]
        {
            let spinner = create_spinner("Extracting entities (tree-sitter)...");
            match repo.kg_enrich_entities() {
                Ok(n) => {
                    total.entities = n;
                    finish_success(&spinner, &format!("{} entit(ies)", n));
                }
                Err(e) => finish_error(&spinner, &format!("entities failed: {}", e)),
            }
        }

        // Phase 6: INCLUDES edges
        #[cfg(feature = "ast")]
        {
            let spinner = create_spinner("Resolving includes...");
            match repo.kg_enrich_includes() {
                Ok(n) => {
                    total.includes = n;
                    finish_success(&spinner, &format!("{} include(s)", n));
                }
                Err(e) => finish_error(&spinner, &format!("includes failed: {}", e)),
            }
        }

        // Phase 7: Content search index (syntext)
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
pub struct Reindex;

impl Command for Reindex {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let count = repo.vault_reindex_kg().map_err(CliError::Repository)?;
        println!("Indexed {} nodes + edges.", count);

        Ok(())
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
/// {"steps":[{"type":"kg_search","query":"auth","limit":5,"bind":"results"}]}
#[derive(Parser, Debug)]
pub struct PlanExec {
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for PlanExec {
    fn run(&self) -> CliResult<()> {
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
                    print!("  [{}] {}", node.kind, node.label);
                    if let Some(ref s) = node.summary {
                        let short = if s.len() > 60 { &s[..60] } else { s };
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
                    let preview = if text.len() > 100 { &text[..100] } else { text };
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
- file nodes (file:path) — tracked files
- entity nodes (entity:file:name:line) — functions, structs, classes from tree-sitter
- change nodes (change:hash) — commit history with dates and messages
- view nodes (view:name) — like branches
- goal/intent/memory nodes — development sessions and work items
- edges: MODIFIES, DEFINES, AUTHORED_BY, DEPENDS_ON, REFERENCES, ON_VIEW, etc.";

impl Command for QueryAsk {
    fn run(&self) -> CliResult<()> {
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
