//! `atomic vault context` — retrieve memories relevant to a task.
//!
//! Assembles a small, ranked bundle of vault memories for injection into
//! an AI agent's prompt at run start (the pre-task retrieval step of the
//! knowledge flywheel). Seeds come from free-text query terms, an intent
//! (`--intent`), or file paths (`--files`); candidates are gathered from
//! the knowledge graph (keyword search + graph neighbors) and ranked by
//! relevance, graph adjacency, and recency.
//!
//! With no seeds at all, the most recently updated memories are returned.
//!
//! # Usage
//!
//! ```text
//! atomic vault context [QUERY]... [OPTIONS]
//!
//! Options:
//!   --intent <ID>         Seed from an intent's title and labels
//!   --files <PATH>        Seed from a file path (repeatable)
//!   --limit <N>           Maximum memories to return [default: 5]
//!   --budget-chars <N>    Total character budget for bodies [default: 8000]
//!   --format <FORMAT>     Output format: md or json [default: md]
//!   --json                Shorthand for --format json
//! ```
//!
//! # Examples
//!
//! ```text
//! # Memories relevant to an intent (typical pre-run injection)
//! atomic vault context --intent PIMO-1
//!
//! # Free-text query
//! atomic vault context "authentication tokens"
//!
//! # Memories that reference a specific file
//! atomic vault context --files src/auth/login.rs
//!
//! # Machine-readable (paths/scores for run sidecars)
//! atomic vault context --intent PIMO-1 --json
//! ```

use std::collections::HashMap;

use clap::Parser;

use atomic_core::pristine::vault::VaultEntryType;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

// Tuning constants

/// Default maximum number of memories returned.
const DEFAULT_LIMIT: usize = 5;

/// Default total character budget across all memory bodies.
const DEFAULT_BUDGET_CHARS: usize = 8000;

/// Fetch this many times `--limit` from the KG search so that filtering
/// to memory nodes still leaves enough candidates.
const SEARCH_POOL_MULTIPLIER: usize = 4;

/// Score bonus for memories directly connected (in the KG) to the seed
/// intent or seed files.
const NEIGHBOR_BONUS: f64 = 0.75;

/// Weight of a full body-term match. The KG FTS only indexes node
/// ids/labels/summaries, so memory *bodies* are matched by a direct
/// scan at this layer; a body matching every seed term scores just
/// below a rank-0 label match.
const BODY_MATCH_WEIGHT: f64 = 0.8;

/// Seed terms shorter than this are ignored by the body scan
/// (stop-word noise).
const MIN_TERM_LEN: usize = 3;

/// Weight of the recency component in the final score.
const RECENCY_WEIGHT: f64 = 0.25;

/// Half-life, in days, of the recency component.
const RECENCY_HALF_LIFE_DAYS: f64 = 90.0;

/// Fenced markers so injectors (and future dedup passes) can find the
/// block — mirrors the `atomic:learnings` markers in CLAUDE.md.
const MARKER_START: &str = "<!-- atomic:memory-context:start -->";
const MARKER_END: &str = "<!-- atomic:memory-context:end -->";

/// Memories whose frontmatter `type` matches one of these are never
/// injected (index/table-of-contents files, not knowledge).
const EXCLUDED_MEMORY_TYPES: &[&str] = &["index"];

// Context Command

/// Retrieve memories relevant to a task, for prompt injection.
#[derive(Parser, Debug)]
#[command(name = "context")]
pub struct Context {
    /// Free-text query terms.
    pub query: Vec<String>,

    /// Seed from an intent: uses its title and labels as query terms and
    /// its graph neighbors as candidates (e.g., "PIMO-1").
    #[arg(long)]
    pub intent: Option<String>,

    /// Seed from a file path; memories referencing it are candidates.
    #[arg(long = "files", value_name = "PATH")]
    pub files: Vec<String>,

    /// Maximum number of memories to return.
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,

    /// Total character budget across all memory bodies.
    #[arg(long, default_value_t = DEFAULT_BUDGET_CHARS)]
    pub budget_chars: usize,

    /// Output format: "md" (injectable block) or "json".
    #[arg(long, default_value = "md")]
    pub format: String,

    /// Output as JSON (shorthand for `--format json`).
    #[arg(long)]
    pub json: bool,
}

impl Command for Context {
    fn run(&self) -> CliResult<()> {
        let as_json = self.json || self.format.eq_ignore_ascii_case("json");
        if !as_json && !self.format.eq_ignore_ascii_case("md") {
            return Err(CliError::InvalidArgument {
                message: format!("Unknown format: {} (expected md or json)", self.format),
            });
        }

        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let limit = self.limit.max(1);
        let candidates = self.gather_candidates(&repo, limit)?;
        let items = resolve_and_rank(&repo, candidates, limit)?;
        let items = apply_budget(items, self.budget_chars);

        if as_json {
            println!("{}", render_json(&items));
        } else {
            let md = render_md(&items);
            if !md.is_empty() {
                print!("{}", md);
            }
        }

        Ok(())
    }
}

impl Context {
    /// Gather candidate memory nodes from all seeds.
    ///
    /// Returns a map of memory node id -> accumulated candidate score.
    /// With no seeds at all, falls back to the most recently updated
    /// memories so a bare `atomic vault context` is still useful.
    fn gather_candidates(
        &self,
        repo: &Repository,
        limit: usize,
    ) -> CliResult<HashMap<String, CandidateScore>> {
        let mut candidates: HashMap<String, CandidateScore> = HashMap::new();
        let mut seed_terms: Vec<String> = self.query.clone();

        if let Some(intent_id) = &self.intent {
            self.seed_from_intent(repo, intent_id, &mut seed_terms, &mut candidates)?;
        }

        for file in &self.files {
            let node_id = format!("file:{}", file.trim_start_matches("./"));
            add_memory_neighbors(repo, &node_id, &mut candidates)?;
        }

        let seed_query = seed_terms.join(" ");
        if !seed_query.trim().is_empty() {
            let pool = limit * SEARCH_POOL_MULTIPLIER;
            let nodes = repo
                .vault_kg_search(&seed_query, pool, None)
                .map_err(CliError::Repository)?;
            for (rank, node) in nodes.iter().filter(|n| n.kind == "memory").enumerate() {
                let base = rank_score(rank);
                candidates
                    .entry(node.id.clone())
                    .and_modify(|c| c.base = c.base.max(base))
                    .or_insert(CandidateScore {
                        base,
                        neighbor: false,
                    });
            }
            self.scan_memory_bodies(repo, &seed_query, &mut candidates)?;
        }

        // No seeds of any kind: fall back to the most recent memories.
        if candidates.is_empty() && seed_query.trim().is_empty() {
            let mut metas = repo
                .vault_list("memory/", Some(VaultEntryType::Memory))
                .map_err(CliError::Repository)?;
            metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            for meta in metas.into_iter().take(limit * 2) {
                if let Some(node_id) = memory_path_to_node_id(&meta.path) {
                    candidates.entry(node_id).or_insert(CandidateScore {
                        base: 0.0,
                        neighbor: false,
                    });
                }
            }
        }

        Ok(candidates)
    }

    /// Match seed terms against memory bodies directly.
    ///
    /// The KG FTS indexes only node ids/labels/summaries, so body words
    /// are invisible to `vault_kg_search`. Memories are few and small;
    /// a linear scan at this layer keeps free-text queries useful
    /// without touching the storage layer.
    fn scan_memory_bodies(
        &self,
        repo: &Repository,
        seed_query: &str,
        candidates: &mut HashMap<String, CandidateScore>,
    ) -> CliResult<()> {
        let terms = search_terms(seed_query);
        if terms.is_empty() {
            return Ok(());
        }
        let metas = repo
            .vault_list("memory/", Some(VaultEntryType::Memory))
            .map_err(CliError::Repository)?;
        for meta in metas {
            let Some(node_id) = memory_path_to_node_id(&meta.path) else {
                continue;
            };
            let Some(entry) = repo
                .vault_retrieve(&meta.path)
                .map_err(CliError::Repository)?
            else {
                continue;
            };
            let body = String::from_utf8_lossy(&entry.content_bytes);
            let matched = body_match_fraction(&terms, &body);
            if matched > 0.0 {
                let base = BODY_MATCH_WEIGHT * matched;
                candidates
                    .entry(node_id)
                    .and_modify(|c| c.base = c.base.max(base))
                    .or_insert(CandidateScore {
                        base,
                        neighbor: false,
                    });
            }
        }
        Ok(())
    }

    /// Seed query terms and graph-neighbor candidates from an intent.
    fn seed_from_intent(
        &self,
        repo: &Repository,
        intent_id: &str,
        seed_terms: &mut Vec<String>,
        candidates: &mut HashMap<String, CandidateScore>,
    ) -> CliResult<()> {
        let manifest = repo.vault_manifest().map_err(CliError::Repository)?;
        let (_, summary) = manifest
            .intents
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(intent_id))
            .ok_or_else(|| CliError::InvalidArgument {
                message: format!("Intent not found: {}", intent_id),
            })?;

        seed_terms.push(summary.title.clone());

        if let Some(entry) = repo
            .vault_retrieve(&summary.vault_path)
            .map_err(CliError::Repository)?
        {
            seed_terms.extend(frontmatter_labels(&entry.frontmatter_json));
        }

        let node_id = intent_path_to_node_id(&summary.vault_path);
        add_memory_neighbors(repo, &node_id, candidates)?;
        Ok(())
    }
}

// Candidate gathering helpers

/// Partial score for a candidate memory node, accumulated across seeds.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CandidateScore {
    /// Rank-derived score from the KG keyword search (0 if graph-only).
    base: f64,
    /// Whether the memory is a direct KG neighbor of a seed node.
    neighbor: bool,
}

/// A memory selected for output.
#[derive(Clone, Debug, PartialEq)]
struct MemoryItem {
    path: String,
    name: String,
    kind: String,
    updated_at: String,
    score: f64,
    body: String,
    truncated: bool,
}

/// Add all memory nodes adjacent to `node_id` as neighbor candidates.
fn add_memory_neighbors(
    repo: &Repository,
    node_id: &str,
    candidates: &mut HashMap<String, CandidateScore>,
) -> CliResult<()> {
    let subgraph = repo
        .vault_kg_neighbors(node_id, 1)
        .map_err(CliError::Repository)?;
    for node in subgraph.nodes {
        if node.kind == "memory" && node.id != node_id {
            candidates
                .entry(node.id)
                .and_modify(|c| c.neighbor = true)
                .or_insert(CandidateScore {
                    base: 0.0,
                    neighbor: true,
                });
        }
    }
    Ok(())
}

/// Load candidate bodies from the vault, score, and rank them.
fn resolve_and_rank(
    repo: &Repository,
    candidates: HashMap<String, CandidateScore>,
    limit: usize,
) -> CliResult<Vec<MemoryItem>> {
    let now = chrono::Utc::now();
    let mut items: Vec<MemoryItem> = Vec::new();

    for (node_id, score) in candidates {
        let Some(path) = memory_node_id_to_path(&node_id) else {
            continue;
        };
        let Some(entry) = repo.vault_retrieve(&path).map_err(CliError::Repository)? else {
            continue; // stale KG node — the entry no longer exists
        };

        let kind = frontmatter_type(&entry.frontmatter_json);
        if EXCLUDED_MEMORY_TYPES.contains(&kind.as_str()) {
            continue;
        }

        let name = node_id.split_once(':').map(|(_, n)| n).unwrap_or(&node_id);
        let recency = recency_score(&entry.updated_at, now);
        items.push(MemoryItem {
            path,
            name: name.to_string(),
            kind,
            updated_at: entry.updated_at.clone(),
            score: final_score(score, recency),
            body: String::from_utf8_lossy(&entry.content_bytes)
                .trim()
                .to_string(),
            truncated: false,
        });
    }

    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    items.truncate(limit);
    Ok(items)
}

// Scoring

/// Rank-derived score for the `rank`-th KG search result (0-based).
fn rank_score(rank: usize) -> f64 {
    1.0 / (1.0 + rank as f64)
}

/// Lowercased, deduplicated seed terms of at least [`MIN_TERM_LEN`] chars.
fn search_terms(seed_query: &str) -> Vec<String> {
    let mut terms: Vec<String> = seed_query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= MIN_TERM_LEN)
        .map(|t| t.to_lowercase())
        .collect();
    terms.sort();
    terms.dedup();
    terms
}

/// Fraction of seed terms found in the body (case-insensitive).
fn body_match_fraction(terms: &[String], body: &str) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let body_lower = body.to_lowercase();
    let matched = terms
        .iter()
        .filter(|t| body_lower.contains(t.as_str()))
        .count();
    matched as f64 / terms.len() as f64
}

/// Recency component in [0, 1]: 1.0 for "just updated", halving every
/// [`RECENCY_HALF_LIFE_DAYS`]. Unparseable timestamps score 0.
fn recency_score(updated_at: &str, now: chrono::DateTime<chrono::Utc>) -> f64 {
    let Ok(updated) = chrono::DateTime::parse_from_rfc3339(updated_at) else {
        return 0.0;
    };
    let age_days = (now - updated.with_timezone(&chrono::Utc)).num_seconds() as f64 / 86_400.0;
    if age_days <= 0.0 {
        return 1.0;
    }
    0.5_f64.powf(age_days / RECENCY_HALF_LIFE_DAYS)
}

/// Combine the candidate score parts into the final ranking score.
fn final_score(candidate: CandidateScore, recency: f64) -> f64 {
    let neighbor = if candidate.neighbor {
        NEIGHBOR_BONUS
    } else {
        0.0
    };
    candidate.base + neighbor + RECENCY_WEIGHT * recency
}

// Node id <-> vault path mapping
//
// Mirrors `entry_subject` in atomic-repository's vault_triples.rs; the
// KG stores memory nodes as `memory:<name>` for `memory/<name>.md` and
// intent nodes as `intent:<UPPERCASED PATH SEGMENT>` for
// `intents/<segment>/intent.md`.

/// `memory:architecture` -> `memory/architecture.md`.
fn memory_node_id_to_path(node_id: &str) -> Option<String> {
    let name = node_id.strip_prefix("memory:")?;
    if name.is_empty() {
        return None;
    }
    Some(format!("memory/{}.md", name))
}

/// `memory/architecture.md` -> `memory:architecture`.
fn memory_path_to_node_id(path: &str) -> Option<String> {
    let name = path.strip_prefix("memory/")?.strip_suffix(".md")?;
    if name.is_empty() {
        return None;
    }
    Some(format!("memory:{}", name))
}

/// `intents/pimo-1/intent.md` -> `intent:PIMO-1` (nested paths keep
/// their inner segments, uppercased, matching the KG indexer).
fn intent_path_to_node_id(path: &str) -> String {
    let id = path
        .strip_prefix("intents/")
        .and_then(|s| s.strip_suffix("/intent.md"))
        .unwrap_or(path)
        .to_uppercase();
    format!("intent:{}", id)
}

// Frontmatter helpers

/// Extract the memory `type` from frontmatter JSON (default: "project").
fn frontmatter_type(frontmatter_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(frontmatter_json)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
        .unwrap_or_else(|| "project".to_string())
}

/// Extract `labels` (array of strings) from frontmatter JSON.
fn frontmatter_labels(frontmatter_json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(frontmatter_json)
        .ok()
        .and_then(|v| {
            v.get("labels").and_then(|l| l.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
        })
        .unwrap_or_default()
}

// Budget

/// Truncate bodies so their combined length fits `budget_chars`,
/// splitting the budget evenly across the selected memories.
fn apply_budget(items: Vec<MemoryItem>, budget_chars: usize) -> Vec<MemoryItem> {
    if items.is_empty() {
        return items;
    }
    let per_item = budget_chars / items.len();
    items
        .into_iter()
        .map(|mut item| {
            if item.body.chars().count() > per_item {
                item.body = truncate_chars(&item.body, per_item);
                item.truncated = true;
            }
            item
        })
        .collect()
}

/// Truncate to at most `max_chars` characters on a char boundary.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

// Rendering

/// Render the injectable markdown block. Empty input renders to an
/// empty string so callers can prepend unconditionally.
fn render_md(items: &[MemoryItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(MARKER_START);
    out.push_str("\n## Relevant memories\n");
    for item in items {
        let date = item.updated_at.get(0..10).unwrap_or(&item.updated_at);
        out.push_str(&format!(
            "\n### {} [{} · {}]\n\n",
            item.name, item.kind, date
        ));
        out.push_str(&item.body);
        if item.truncated {
            out.push_str("\n\n_[truncated]_");
        }
        out.push('\n');
    }
    out.push_str(MARKER_END);
    out.push('\n');
    out
}

/// Render the JSON array (`[{path, name, kind, score, preview, updated_at}]`).
fn render_json(items: &[MemoryItem]) -> String {
    let values: Vec<serde_json::Value> = items
        .iter()
        .map(|item| {
            serde_json::json!({
                "path": item.path,
                "name": item.name,
                "kind": item.kind,
                "score": (item.score * 1000.0).round() / 1000.0,
                "preview": preview(&item.body),
                "updated_at": item.updated_at,
            })
        })
        .collect();
    serde_json::to_string_pretty(&values).unwrap_or_else(|_| "[]".to_string())
}

/// One-line preview of a body (first 160 chars, newlines collapsed).
fn preview(body: &str) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flat, 160)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, score: f64, body: &str) -> MemoryItem {
        MemoryItem {
            path: format!("memory/{}.md", name),
            name: name.to_string(),
            kind: "project".to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            score,
            body: body.to_string(),
            truncated: false,
        }
    }

    #[test]
    fn test_memory_node_id_to_path() {
        assert_eq!(
            memory_node_id_to_path("memory:architecture"),
            Some("memory/architecture.md".to_string())
        );
        assert_eq!(memory_node_id_to_path("memory:"), None);
        assert_eq!(memory_node_id_to_path("file:src/main.rs"), None);
    }

    #[test]
    fn test_memory_path_to_node_id() {
        assert_eq!(
            memory_path_to_node_id("memory/architecture.md"),
            Some("memory:architecture".to_string())
        );
        assert_eq!(memory_path_to_node_id("intents/x/intent.md"), None);
    }

    #[test]
    fn test_intent_path_to_node_id_flat() {
        assert_eq!(
            intent_path_to_node_id("intents/pimo-1/intent.md"),
            "intent:PIMO-1"
        );
    }

    #[test]
    fn test_intent_path_to_node_id_nested() {
        // Hook-created intents nest view/session/turn segments; the KG
        // indexer keeps them (uppercased) in the node id.
        assert_eq!(
            intent_path_to_node_id("intents/tight-storm/019e/1/intent.md"),
            "intent:TIGHT-STORM/019E/1"
        );
    }

    #[test]
    fn test_rank_score_decreases() {
        assert!(rank_score(0) > rank_score(1));
        assert!(rank_score(1) > rank_score(5));
        assert_eq!(rank_score(0), 1.0);
    }

    #[test]
    fn test_recency_score_fresh_vs_old() {
        let now = chrono::Utc::now();
        let fresh = now.to_rfc3339();
        let old = (now - chrono::Duration::days(365)).to_rfc3339();
        assert!(recency_score(&fresh, now) > 0.99);
        assert!(recency_score(&old, now) < 0.1);
        assert_eq!(recency_score("not-a-date", now), 0.0);
    }

    #[test]
    fn test_recency_score_half_life() {
        let now = chrono::Utc::now();
        let half = (now - chrono::Duration::days(RECENCY_HALF_LIFE_DAYS as i64)).to_rfc3339();
        let score = recency_score(&half, now);
        assert!((score - 0.5).abs() < 0.01, "half-life score was {}", score);
    }

    #[test]
    fn test_final_score_neighbor_bonus() {
        let base_only = final_score(
            CandidateScore {
                base: 0.5,
                neighbor: false,
            },
            0.0,
        );
        let with_neighbor = final_score(
            CandidateScore {
                base: 0.5,
                neighbor: true,
            },
            0.0,
        );
        assert!((with_neighbor - base_only - NEIGHBOR_BONUS).abs() < f64::EPSILON);
    }

    #[test]
    fn test_search_terms_filters_and_dedupes() {
        assert_eq!(
            search_terms("Fix the JWT-token fix in AUTH"),
            vec!["auth", "fix", "jwt", "the", "token"]
        );
        assert!(search_terms("a an").is_empty());
    }

    #[test]
    fn test_body_match_fraction() {
        let terms = search_terms("cargo clippy");
        let body = "Workflow tip: cargo test is faster; run cargo clippy before pushing.";
        assert_eq!(body_match_fraction(&terms, body), 1.0);
        let half = search_terms("cargo missing");
        assert_eq!(body_match_fraction(&half, body), 0.5);
        assert_eq!(body_match_fraction(&[], body), 0.0);
    }

    #[test]
    fn test_body_match_fraction_case_insensitive() {
        let terms = search_terms("RS256");
        assert_eq!(body_match_fraction(&terms, "uses rs256 signing"), 1.0);
    }

    #[test]
    fn test_frontmatter_type_default_and_explicit() {
        assert_eq!(frontmatter_type("{}"), "project");
        assert_eq!(frontmatter_type("not json"), "project");
        assert_eq!(
            frontmatter_type(r#"{"name":"x","type":"reference"}"#),
            "reference"
        );
    }

    #[test]
    fn test_frontmatter_labels() {
        assert_eq!(
            frontmatter_labels(r#"{"labels":["auth","backend"]}"#),
            vec!["auth".to_string(), "backend".to_string()]
        );
        assert!(frontmatter_labels("{}").is_empty());
        assert!(frontmatter_labels("not json").is_empty());
    }

    #[test]
    fn test_apply_budget_truncates_evenly() {
        let items = vec![
            item("a", 1.0, &"x".repeat(100)),
            item("b", 0.5, &"y".repeat(100)),
        ];
        let out = apply_budget(items, 100);
        assert_eq!(out[0].body.chars().count(), 50);
        assert!(out[0].truncated);
        assert_eq!(out[1].body.chars().count(), 50);
        assert!(out[1].truncated);
    }

    #[test]
    fn test_apply_budget_no_truncation_needed() {
        let items = vec![item("a", 1.0, "short")];
        let out = apply_budget(items, 8000);
        assert_eq!(out[0].body, "short");
        assert!(!out[0].truncated);
    }

    #[test]
    fn test_truncate_chars_multibyte_safe() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("知识飞轮", 2), "知识");
    }

    #[test]
    fn test_render_md_empty() {
        assert_eq!(render_md(&[]), "");
    }

    #[test]
    fn test_render_md_block_shape() {
        let md = render_md(&[item("arch", 1.0, "Uses RS256, not HS256.")]);
        assert!(md.starts_with(MARKER_START));
        assert!(md.trim_end().ends_with(MARKER_END));
        assert!(md.contains("### arch [project · 2026-07-01]"));
        assert!(md.contains("Uses RS256, not HS256."));
        assert!(!md.contains("_[truncated]_"));
    }

    #[test]
    fn test_render_md_truncation_marker() {
        let mut it = item("arch", 1.0, "body");
        it.truncated = true;
        assert!(render_md(&[it]).contains("_[truncated]_"));
    }

    #[test]
    fn test_render_json_shape() {
        let json = render_json(&[item("arch", 0.12345, "line one\nline two")]);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["path"], "memory/arch.md");
        assert_eq!(parsed[0]["name"], "arch");
        assert_eq!(parsed[0]["kind"], "project");
        assert_eq!(parsed[0]["score"], 0.123);
        assert_eq!(parsed[0]["preview"], "line one line two");
    }

    #[test]
    fn test_render_json_empty() {
        assert_eq!(render_json(&[]), "[]");
    }

    #[test]
    fn test_preview_collapses_whitespace_and_caps() {
        let long = format!("a  b\n\nc {}", "z".repeat(500));
        let p = preview(&long);
        assert!(p.starts_with("a b c"));
        assert_eq!(p.chars().count(), 160);
    }
}
