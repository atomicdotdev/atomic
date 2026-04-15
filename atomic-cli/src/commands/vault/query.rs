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

    /// Filter by node kind (change, entity, file, view).
    ///
    /// When set, only nodes of this kind are returned.
    /// Without this flag, all kinds are returned.
    #[arg(long, short = 't')]
    pub kind: Option<String>,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryNodes {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Fetch more results when filtering so we have enough after the filter
        let fetch_limit = if self.kind.is_some() {
            self.limit * 10
        } else {
            self.limit
        };

        let all_nodes = repo
            .vault_kg_search(&self.query, fetch_limit)
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

    /// Maximum KG nodes to include in context.
    #[arg(long, short = 'k', default_value = "30")]
    pub context_limit: usize,

    /// Lines of context around grep matches.
    #[arg(long, short = 'C', default_value = "5")]
    pub context_lines: usize,

    /// Maximum source context bytes to include in the prompt.
    #[arg(long, default_value = "24000")]
    pub max_source_bytes: usize,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for QueryAsk {
    fn run(&self) -> CliResult<()> {
        use std::collections::{BTreeMap, HashMap, HashSet};
        use std::io::{BufRead, BufReader};
        use std::process::Command as ProcessCommand;

        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // ── Step 1: Tokenize the question into search terms ────────────
        let search_terms = extract_search_terms(&self.question);

        // ── Step 2: KG search per term → bucket by node kind ───────────
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut kind_map: BTreeMap<String, Vec<atomic_core::pristine::vault::KgNode>> =
            BTreeMap::new();

        let per_term_limit = (self.context_limit / search_terms.len().max(1)).max(5);

        for term in &search_terms {
            if let Ok(results) = repo.vault_kg_search(term, per_term_limit) {
                for node in results {
                    if seen_ids.insert(node.id.clone()) {
                        kind_map.entry(node.kind.clone()).or_default().push(node);
                    }
                }
            }
        }

        // Also search the full question for phrase-level matches
        if let Ok(results) = repo.vault_kg_search(&self.question, per_term_limit) {
            for node in results {
                if seen_ids.insert(node.id.clone()) {
                    kind_map.entry(node.kind.clone()).or_default().push(node);
                }
            }
        }

        // Collect edges for matched nodes (1-hop neighborhood)
        let mut all_edges = Vec::new();
        let mut seen_edges: HashSet<(String, String, String)> = HashSet::new();
        for nodes in kind_map.values() {
            for node in nodes {
                if let Ok(sg) = repo.vault_kg_neighbors(&node.id, 1) {
                    for edge in sg.edges {
                        let key = (edge.from_id.clone(), edge.to_id.clone(), edge.kind.clone());
                        if seen_edges.insert(key) {
                            all_edges.push(edge);
                        }
                    }
                }
            }
        }

        // ── Step 3: Collect unique file paths from entity + file nodes ─
        let mut file_paths: Vec<String> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();

        // From entity nodes
        if let Some(entities) = kind_map.get("entity") {
            for node in entities {
                if let Some(ref meta) = node.metadata {
                    if let Some(file) = meta.get("file").and_then(|v| v.as_str()) {
                        if seen_files.insert(file.to_string()) {
                            file_paths.push(file.to_string());
                        }
                    }
                }
            }
        }

        // From file nodes
        if let Some(files) = kind_map.get("file") {
            for node in files {
                let path = if node.id.starts_with("file:") {
                    &node.id[5..]
                } else {
                    &node.id
                };
                if seen_files.insert(path.to_string()) {
                    file_paths.push(path.to_string());
                }
            }
        }

        // ── Step 4: Grep source files for search terms with context ────
        //
        // For each file, run grep with the search terms to get actual
        // source code in context.  This gives the LLM real implementation
        // code, not just signatures from entity metadata.
        let mut source_snippets: Vec<String> = Vec::new();
        let mut source_bytes = 0usize;

        // Build a grep pattern from the search terms (OR them together)
        let grep_pattern = search_terms.join("|");

        if !grep_pattern.is_empty() && !file_paths.is_empty() {
            // Use grep on each file individually so we can attribute snippets
            for file_path in &file_paths {
                if source_bytes >= self.max_source_bytes {
                    break;
                }

                let abs_path = root.join(file_path);
                if !abs_path.exists() || abs_path.is_dir() {
                    continue;
                }

                // Skip very large files (>1MB) to avoid blowing up context
                if let Ok(meta) = std::fs::metadata(&abs_path) {
                    if meta.len() > 1_000_000 {
                        continue;
                    }
                }

                let output = match ProcessCommand::new("grep")
                    .args([
                        "-n",                                 // line numbers
                        "-i",                                 // case insensitive
                        "-E",                                 // extended regex
                        &format!("-C{}", self.context_lines), // context lines
                        &grep_pattern,
                    ])
                    .arg(&abs_path)
                    .output()
                {
                    Ok(o) if !o.stdout.is_empty() => o,
                    _ => continue,
                };

                let reader = BufReader::new(output.stdout.as_slice());
                let mut file_snippet = format!("// {}\n", file_path);
                let mut snippet_bytes = file_snippet.len();

                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => continue,
                    };
                    if source_bytes + snippet_bytes + line.len() + 1 > self.max_source_bytes {
                        break;
                    }
                    file_snippet.push_str(&line);
                    file_snippet.push('\n');
                    snippet_bytes += line.len() + 1;
                }

                if snippet_bytes > file_path.len() + 5 {
                    source_bytes += snippet_bytes;
                    source_snippets.push(file_snippet);
                }
            }
        }

        // ── Step 5: Build the structured prompt ────────────────────────
        let all_nodes: Vec<&atomic_core::pristine::vault::KgNode> = kind_map
            .values()
            .flat_map(|v| v.iter())
            .take(self.context_limit)
            .collect();

        let mut context = String::with_capacity(self.max_source_bytes + 4000);
        context.push_str(&format!(
            "Repository context for: \"{}\"\n\n",
            self.question
        ));

        // KG structure grouped by kind
        for (kind, nodes) in &kind_map {
            let display_kind = capitalize(kind);
            context.push_str(&format!("{}:\n", display_kind));

            for node in nodes.iter().take(self.context_limit) {
                context.push_str(&format!("- {}", node.label));
                if let Some(ref summary) = node.summary {
                    let short = if summary.len() > 120 {
                        format!("{}...", &summary[..120])
                    } else {
                        summary.clone()
                    };
                    context.push_str(&format!(": {}", short));
                }
                // Entity metadata (kind, file, line)
                if node.kind == "entity" {
                    if let Some(ref meta) = node.metadata {
                        let mut details = Vec::new();
                        if let Some(k) = meta.get("kind").and_then(|v| v.as_str()) {
                            details.push(k.to_string());
                        }
                        if let Some(f) = meta.get("file").and_then(|v| v.as_str()) {
                            if let Some(line) = meta.get("line").and_then(|v| v.as_u64()) {
                                details.push(format!("{}:{}", f, line));
                            } else {
                                details.push(f.to_string());
                            }
                        }
                        if !details.is_empty() {
                            context.push_str(&format!(" [{}]", details.join(", ")));
                        }
                    }
                }
                // Change metadata (date, sequence)
                if node.kind == "change" {
                    if let Some(ref meta) = node.metadata {
                        let mut details = Vec::new();
                        if let Some(ts) = meta.get("timestamp").and_then(|v| v.as_str()) {
                            details.push(format!("date: {}", ts));
                        }
                        if let Some(seq) = meta.get("sequence").and_then(|v| v.as_u64()) {
                            details.push(format!("#{}", seq));
                        }
                        if !details.is_empty() {
                            context.push_str(&format!(" ({})", details.join(", ")));
                        }
                    }
                }
                context.push('\n');
            }
            context.push('\n');
        }

        // Relationships
        if !all_edges.is_empty() {
            context.push_str("Relationships:\n");
            for edge in all_edges.iter().take(50) {
                context.push_str(&format!(
                    "- {} -[{}]-> {}\n",
                    edge.from_id, edge.kind, edge.to_id
                ));
            }
            context.push('\n');
        }

        // Source code from grep
        if !source_snippets.is_empty() {
            context.push_str("Source code (grep matches with context):\n\n");
            for snippet in &source_snippets {
                context.push_str(snippet);
                context.push_str("\n---\n\n");
            }
        }

        // ── Step 6: Send to LLM ───────────────────────────────────────
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
            let owned_nodes: Vec<_> = all_nodes.into_iter().cloned().collect();
            let json = serde_json::json!({
                "query": self.question,
                "search_terms": search_terms,
                "nodes_by_kind": kind_map.iter()
                    .map(|(k, v)| (k.clone(), v.len()))
                    .collect::<HashMap<String, usize>>(),
                "files_grepped": file_paths.len(),
                "source_snippets": source_snippets.len(),
                "source_bytes": source_bytes,
                "nodes": owned_nodes,
                "edges": all_edges,
                "answer": answer.as_ref().map(|a| &a.answer),
                "model": answer.as_ref().map(|a| &a.model),
                "tokens_used": answer.as_ref().and_then(|a| a.tokens_used),
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else if let Some(ref resp) = answer {
            println!("{}\n", resp.answer);
            println!(
                "  — {} ({}, {} files, {}KB context)",
                resp.model,
                resp.tokens_used
                    .map(|t| format!("{} tokens", t))
                    .unwrap_or_default(),
                file_paths.len(),
                source_bytes / 1024,
            );
        } else if kind_map.is_empty() {
            println!("No results found.");
        } else {
            println!("Results (no API key set for LLM answer):\n");
            for (kind, nodes) in &kind_map {
                println!("  {}:", capitalize(kind));
                for node in nodes.iter().take(10) {
                    print!("    {}", node.label);
                    if let Some(ref s) = node.summary {
                        let short = if s.len() > 80 {
                            format!("{}...", &s[..80])
                        } else {
                            s.clone()
                        };
                        print!(": {}", short);
                    }
                    println!();
                }
            }
        }

        Ok(())
    }
}

/// Extract meaningful search terms from a natural language question.
///
/// Extracts individual keywords (filtered for stop words) and adjacent
/// bigrams.  Longer words are prioritized (more specific).  No cap on
/// term count — all meaningful terms are returned so the KG search has
/// maximum recall.
fn extract_search_terms(question: &str) -> Vec<String> {
    let stop_words: std::collections::HashSet<&str> = [
        "a",
        "an",
        "and",
        "are",
        "as",
        "at",
        "be",
        "but",
        "by",
        "do",
        "does",
        "for",
        "from",
        "had",
        "has",
        "have",
        "how",
        "if",
        "in",
        "into",
        "is",
        "it",
        "its",
        "let",
        "my",
        "no",
        "not",
        "of",
        "on",
        "or",
        "our",
        "so",
        "than",
        "that",
        "the",
        "their",
        "them",
        "then",
        "there",
        "these",
        "they",
        "this",
        "to",
        "us",
        "using",
        "was",
        "we",
        "what",
        "when",
        "where",
        "which",
        "who",
        "why",
        "will",
        "with",
        "would",
        "you",
        "your",
        "most",
        "recent",
        "can",
        "could",
        "should",
        "about",
        "also",
        "just",
        "like",
        "more",
        "some",
        "such",
        "very",
        "all",
        "any",
        "each",
        "every",
        "much",
        "own",
        "describe",
        "explain",
        "show",
        "tell",
        "find",
        "list",
        "give",
        "get",
        "make",
        "many",
        "use",
        "used",
        "implement",
        "implements",
        "class",
        "classes",
        "function",
        "functions",
        "method",
        "methods",
        "file",
        "files",
    ]
    .iter()
    .copied()
    .collect();

    let words: Vec<String> = question
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '#' && c != '.')
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_lowercase())
        .filter(|w| !stop_words.contains(w.as_str()))
        .collect();

    let mut terms = Vec::new();

    // Individual keywords (most specific first — longer words)
    let mut sorted_words = words.clone();
    sorted_words.sort_by_key(|b| std::cmp::Reverse(b.len()));
    sorted_words.dedup();
    for word in &sorted_words {
        terms.push(word.clone());
    }

    // Bigrams from adjacent words (preserves phrase structure)
    for pair in words.windows(2) {
        let bigram = format!("{} {}", pair[0], pair[1]);
        if !terms.contains(&bigram) {
            terms.push(bigram);
        }
    }

    terms
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}
