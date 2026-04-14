//! `atomic vault query` — query the vault knowledge graph.

use clap::{Parser, Subcommand};

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

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
            QueryCommands::Embed(cmd) => cmd.run(),
            QueryCommands::Reindex(cmd) => cmd.run(),
            QueryCommands::Enrich(cmd) => cmd.run(),
            QueryCommands::Ask(cmd) => cmd.run(),
            QueryCommands::Plan(cmd) => cmd.run(),
        }
    }
}

/// Search the knowledge graph.
#[derive(Parser, Debug)]
pub struct QueryNodes {
    /// Search query text (keyword search over node labels and summaries).
    pub query: String,

    /// Maximum results.
    #[arg(long, short = 'k', default_value = "10")]
    pub limit: usize,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryNodes {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let nodes = repo
            .vault_kg_search(&self.query, self.limit)
            .map_err(CliError::Repository)?;

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

/// Rebuild the KG from VCS data (changes, files, views, dependencies).
#[derive(Parser, Debug)]
pub struct QueryEnrich;

impl Command for QueryEnrich {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let stats = repo.kg_enrich_from_vcs().map_err(CliError::Repository)?;
        println!("KG enriched: {}", stats);

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
#[derive(Parser, Debug)]
pub struct QueryAsk {
    /// Natural language question.
    pub question: String,

    /// Maximum KG nodes to include in context.
    #[arg(long, short = 'k', default_value = "10")]
    pub context_limit: usize,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryAsk {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Step 1: FTS search (fetch extra candidates, then truncate)
        let nodes = repo
            .vault_kg_search(&self.question, self.context_limit * 3)
            .map_err(CliError::Repository)?;

        // Step 2: Collect edges for matched nodes
        let mut all_edges = Vec::new();
        let mut seen_edges = std::collections::HashSet::new();
        for node in &nodes {
            if let Ok(sg) = repo.vault_kg_neighbors(&node.id, 1) {
                for edge in sg.edges {
                    let key = (edge.from_id.clone(), edge.to_id.clone(), edge.kind.clone());
                    if seen_edges.insert(key) {
                        all_edges.push(edge);
                    }
                }
            }
        }

        // Truncate nodes to context_limit
        let top_nodes: Vec<_> = nodes.into_iter().take(self.context_limit).collect();

        // Step 3: Try LLM answer
        let context =
            atomic_repository::build_context_string(&top_nodes, &all_edges, &self.question);

        let llm = atomic_repository::resolve_llm_provider();
        let answer = if let Some(ref provider) = llm {
            match provider.answer_sync(&context, &self.question) {
                Ok(resp) => Some(resp),
                Err(e) => {
                    eprintln!("LLM call failed: {} (showing search results only)", e);
                    None
                }
            }
        } else {
            None
        };

        if self.json {
            let json = serde_json::json!({
                "query": self.question,
                "nodes": top_nodes,
                "edges": all_edges,
                "answer": answer.as_ref().map(|a| &a.answer),
                "model": answer.as_ref().map(|a| &a.model),
                "tokens_used": answer.as_ref().and_then(|a| a.tokens_used),
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else if let Some(ref resp) = answer {
            println!("{}\n", resp.answer);
            println!(
                "  — {} ({})",
                resp.model,
                resp.tokens_used
                    .map(|t| format!("{} tokens", t))
                    .unwrap_or_default()
            );
        } else if top_nodes.is_empty() {
            println!("No results found.");
        } else {
            println!("Results (no API key set for LLM answer):\n");
            for node in &top_nodes {
                print!("  [{}] {}", node.kind, node.label);
                if let Some(ref s) = node.summary {
                    print!(": {}", s);
                }
                println!();
            }
        }

        Ok(())
    }
}
