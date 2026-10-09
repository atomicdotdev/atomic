//! Vault, attestation, knowledge, and turn-dispatch handlers — the
//! agent-facing product surface of the full-turn slice.

use std::sync::Arc;

use crate::atomic::attestation_target::Kind as TargetKind;
use crate::atomic::query_graph_request::Query as QueryOneof;
use crate::atomic::update_vault_entity_request::Update as UpdateKind;
use crate::atomic::verify_attestation_request;
use crate::atomic::ErrorCode;
use crate::atomic::VaultEntityKind as Kind;
use crate::atomic::*;
use atomic_agent::event::TurnEvent;
use atomic_agent::turn::orchestrator::TurnOrchestrator;
use atomic_canonical::did::did_for_public_key;
use atomic_canonical::gate::{validate_intent, validate_memory};
use atomic_canonical::lift::lift_intent;
use atomic_canonical::memory::lift_memory;
use atomic_canonical::proof;
use atomic_canonical::{
    lift_and_attest, lift_and_attest_memory, verify, verify_memory, CanonicalNode, MemoryNode,
};
use atomic_core::change::attestation::Attestation;
use atomic_core::pristine::VaultEntryType;
use atomic_core::types::Base32;
use atomic_identity::signing::Signature;
use atomic_identity::{Identity, IdentityStore};
use atomic_repository::IntentCreateOptions;
use atomic_repository::IntentUpdateOptions;
use atomic_repository::Repository;
use serde_json::{Map, Value};
use tonic::{Request, Response, Status};

use super::convert::*;
use super::journal_sink::DirectJournalSink;
use super::state::{domain_status, repository_error, DaemonState};

// ---------------------------------------------------------------------------
// VaultService
// ---------------------------------------------------------------------------

// The memory-context gather+rank — the domain computation behind
// `atomic vault context`, ported from the CLI's retrieval layer so the
// ranking recipe lives in exactly one place (the handler) and both
// transports serve it. The CLI renders; it no longer ranks.

/// Tuning constants, mirroring the CLI retrieval layer's documented recipe.
mod context_ranking {
    use atomic_core::pristine::tables::tokenize_for_fts;
    use atomic_core::pristine::vault::{KgNode, VaultEntry, VaultEntryType};
    use atomic_core::types::{Base32, Hasher};
    use atomic_repository::Repository;
    use std::collections::{HashMap, HashSet};

    use crate::atomic::{ContextItem, ErrorCode};
    use crate::daemon::state::{domain_status, repository_error};
    use tonic::Status;

    /// Fetch this many times `--limit` from typed KG search so body and
    /// graph signals can rerank a useful candidate pool.
    pub const SEARCH_POOL_MULTIPLIER: usize = 4;

    /// Score bonus for memories directly connected (in the KG) to a seed.
    pub const NEIGHBOR_BONUS: f64 = 0.75;

    /// Weight of a full body-term match: memory *bodies* are matched by a
    /// direct scan (the KG FTS indexes ids/labels/summaries only).
    pub const BODY_MATCH_WEIGHT: f64 = 0.8;

    /// Meaningful short terms the shared FTS tokenizer drops (<3 chars).
    pub const SHORT_TECH_TERMS: &[&str] = &["ai", "go", "r", "c"];

    /// Weight of the recency component in the final score.
    pub const RECENCY_WEIGHT: f64 = 0.25;

    /// Half-life, in days, of the recency component.
    pub const RECENCY_HALF_LIFE_DAYS: f64 = 90.0;

    /// Cap on intent body text used as retrieval terms.
    pub const INTENT_BODY_SEED_CHARS: usize = 2000;

    /// Memories whose frontmatter `type` matches one of these are never
    /// injected (index/table-of-contents files, not knowledge).
    pub const EXCLUDED_MEMORY_TYPES: &[&str] = &["index"];

    /// Partial score for a candidate memory node, accumulated across seeds.
    #[derive(Clone, Debug, PartialEq)]
    pub struct CandidateScore {
        /// Rank-derived score from the KG keyword search (0 if graph-only).
        pub base: f64,
        /// Whether the memory is a direct KG neighbor of a seed node.
        pub neighbor: bool,
        /// Vault path carried by KG metadata or a vault listing.
        pub path: Option<String>,
    }

    pub fn merge_candidate(
        candidates: &mut HashMap<String, CandidateScore>,
        node_id: &str,
        path: Option<String>,
        base: f64,
        neighbor: bool,
    ) {
        candidates
            .entry(node_id.to_string())
            .and_modify(|candidate| {
                candidate.base = candidate.base.max(base);
                candidate.neighbor |= neighbor;
                if candidate.path.is_none() {
                    candidate.path = path.clone();
                }
            })
            .or_insert(CandidateScore {
                base,
                neighbor,
                path,
            });
    }

    /// Add all memory nodes adjacent to `node_id` as neighbor candidates.
    fn add_memory_neighbors(
        repo: &Repository,
        node_id: &str,
        depth: u8,
        candidates: &mut HashMap<String, CandidateScore>,
    ) -> Result<(), Status> {
        let subgraph = repo
            .vault_kg_neighbors(node_id, depth)
            .map_err(repository_error)?;
        for node in subgraph.nodes {
            if node.kind == "memory" && node.id != node_id {
                merge_candidate(
                    candidates,
                    &node.id,
                    memory_path_from_node(&node),
                    0.0,
                    true,
                );
            }
        }
        Ok(())
    }

    /// Rank-derived score for the `rank`-th KG search result (0-based).
    fn rank_score(rank: usize) -> f64 {
        1.0 / (1.0 + rank as f64)
    }

    /// Lowercased, deduplicated terms using KG FTS rules plus a small
    /// allowlist of meaningful short technology names. Exact token
    /// matching prevents substring matches such as `rust` in `trust`.
    fn search_terms(seed_query: &str) -> Vec<String> {
        let mut terms = tokenize_for_fts(seed_query);
        terms.extend(
            seed_query
                .split(|character: char| !character.is_alphanumeric() && character != '_')
                .map(str::to_lowercase)
                .filter(|term| SHORT_TECH_TERMS.contains(&term.as_str())),
        );
        terms.sort();
        terms.dedup();
        terms
    }

    /// Fraction of seed terms present as exact body tokens.
    fn body_match_fraction(terms: &[String], body: &str) -> f64 {
        if terms.is_empty() {
            return 0.0;
        }
        let body_tokens: HashSet<String> = search_terms(body).into_iter().collect();
        let matched = terms
            .iter()
            .filter(|term| body_tokens.contains(*term))
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
    fn final_score(candidate: &CandidateScore, recency: f64) -> f64 {
        let neighbor = if candidate.neighbor {
            NEIGHBOR_BONUS
        } else {
            0.0
        };
        candidate.base + neighbor + RECENCY_WEIGHT * recency
    }

    fn why_matched(candidate: &CandidateScore) -> &'static str {
        match (candidate.base > 0.0, candidate.neighbor) {
            (true, true) => "task terms and knowledge-graph relationship",
            (true, false) => "task terms",
            (false, true) => "knowledge-graph relationship",
            (false, false) => "recent-memory fallback",
        }
    }

    /// Read a vault path from KG node metadata; the legacy fallback maps the
    /// node identity onto ``memory/<name>.md``.
    fn memory_path_from_node(node: &KgNode) -> Option<String> {
        node.metadata
            .as_ref()
            .and_then(|metadata| metadata.get("vault_path"))
            .and_then(|path| path.as_str())
            .filter(|path| path.starts_with("memory/") && path.ends_with(".md"))
            .map(String::from)
            .or_else(|| legacy_memory_node_id_to_path(&node.id))
    }

    /// Legacy fallback: `memory:architecture` -> `memory/architecture.md`.
    fn legacy_memory_node_id_to_path(node_id: &str) -> Option<String> {
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

    // Frontmatter helpers (the stored entries carry JSON-object strings).

    /// The canonical `memoryKind`, accepting legacy `kind`/`type` fields.
    fn frontmatter_kind(frontmatter_json: &str) -> String {
        serde_json::from_str::<serde_json::Value>(frontmatter_json)
            .ok()
            .and_then(|v| {
                ["memoryKind", "kind", "type"]
                    .iter()
                    .find_map(|key| v.get(key).and_then(|value| value.as_str()))
                    .map(String::from)
            })
            .unwrap_or_else(|| "project".to_string())
    }

    fn frontmatter_status(frontmatter_json: &str) -> Option<String> {
        let value = serde_json::from_str::<serde_json::Value>(frontmatter_json).ok()?;
        match value.get("status") {
            None => Some("active".to_string()),
            Some(serde_json::Value::String(status)) if !status.is_empty() => Some(status.clone()),
            Some(_) => None,
        }
    }

    fn frontmatter_string(frontmatter_json: &str, keys: &[&str]) -> Option<String> {
        let value = serde_json::from_str::<serde_json::Value>(frontmatter_json).ok()?;
        keys.iter()
            .find_map(|key| value.get(key).and_then(|item| item.as_str()))
            .filter(|item| !item.is_empty())
            .map(String::from)
    }

    fn frontmatter_name(frontmatter_json: &str) -> Option<String> {
        frontmatter_string(frontmatter_json, &["name"])
    }

    fn frontmatter_memory_id(frontmatter_json: &str) -> Option<String> {
        let id = frontmatter_string(frontmatter_json, &["@id", "uid", "id"])?;
        if id.starts_with("urn:atomic:") {
            Some(id)
        } else {
            Some(format!("urn:atomic:memory:{id}"))
        }
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

    fn is_eligible_memory(entry: &VaultEntry) -> bool {
        if entry.entry_type != VaultEntryType::Memory {
            return false;
        }
        let kind = frontmatter_kind(&entry.frontmatter_json);
        if EXCLUDED_MEMORY_TYPES.contains(&kind.as_str()) {
            return false;
        }
        frontmatter_status(&entry.frontmatter_json)
            .is_some_and(|status| status.eq_ignore_ascii_case("active"))
    }

    /// Identify the exact stored Vault revision (entry type, frontmatter,
    /// and body — not only the body content hash).
    fn vault_entry_revision_hash(entry: &VaultEntry) -> String {
        fn append_revision_field(hasher: &mut Hasher, value: &[u8]) {
            hasher.update(&(value.len() as u64).to_be_bytes());
            hasher.update(value);
        }
        let mut hasher = Hasher::new();
        hasher.update(b"atomic-vault-revision-v1");
        append_revision_field(&mut hasher, entry.entry_type.as_str().as_bytes());
        append_revision_field(&mut hasher, entry.frontmatter_json.as_bytes());
        append_revision_field(&mut hasher, &entry.content_bytes);
        hasher.finalize().to_base32()
    }

    /// Truncate to at most `max_chars` characters on a char boundary.
    fn truncate_chars(value: &str, max_chars: usize) -> String {
        value.chars().take(max_chars).collect()
    }

    /// One-line preview of a body (first 160 chars, newlines collapsed).
    fn preview(body: &str) -> String {
        let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        truncate_chars(&flat, 160)
    }

    /// Gather candidate memory nodes from all seeds: an intent's terms and
    /// graph neighbors, file neighbors, keyword search, body-term matches.
    /// With no seeds at all, falls back to the most recently updated
    /// memories so a bare context request is still useful.
    fn gather_candidates(
        repo: &Repository,
        query: &[String],
        intent: Option<&str>,
        files: &[String],
        limit: usize,
    ) -> Result<HashMap<String, CandidateScore>, Status> {
        let mut candidates: HashMap<String, CandidateScore> = HashMap::new();
        let mut seed_terms: Vec<String> = query.to_vec();
        let mut has_explicit_seeds =
            query.iter().any(|term| !term.trim().is_empty()) || intent.is_some();

        if let Some(intent_id) = intent {
            seed_from_intent(repo, intent_id, &mut seed_terms, &mut candidates)?;
            has_explicit_seeds = true;
        }

        for file in files {
            let node_id = format!("file:{}", file.trim_start_matches("./"));
            add_memory_neighbors(repo, &node_id, 1, &mut candidates)?;
            has_explicit_seeds = true;
        }

        let seed_query = seed_terms.join(" ");
        if !seed_query.trim().is_empty() {
            let pool = limit.saturating_mul(SEARCH_POOL_MULTIPLIER);
            let nodes = repo
                .vault_kg_search_by_kind(&seed_query, pool, "memory")
                .map_err(repository_error)?;
            for (rank, node) in nodes.iter().enumerate() {
                let base = rank_score(rank);
                merge_candidate(
                    &mut candidates,
                    &node.id,
                    memory_path_from_node(node),
                    base,
                    false,
                );
            }
            scan_memory_bodies(repo, &seed_query, &mut candidates)?;
        }

        // No seeds of any kind: fall back to the most recent memories.
        if candidates.is_empty() && !has_explicit_seeds {
            let mut metas = repo
                .vault_list("memory/", Some(VaultEntryType::Memory))
                .map_err(repository_error)?;
            metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            for meta in metas {
                let Some(entry) = repo.vault_retrieve(&meta.path).map_err(repository_error)? else {
                    continue;
                };
                if !is_eligible_memory(&entry) {
                    continue;
                }
                if let Some(node_id) = memory_path_to_node_id(&meta.path) {
                    merge_candidate(&mut candidates, &node_id, Some(meta.path), 0.0, false);
                }
                if candidates.len() >= limit {
                    break;
                }
            }
        }

        Ok(candidates)
    }

    /// Match seed terms against memory bodies directly — the KG FTS
    /// indexes only node ids/labels/summaries, so body words are invisible
    /// to the keyword search. Memories are few and small; a linear scan
    /// keeps free-text queries useful without a storage-layer change.
    fn scan_memory_bodies(
        repo: &Repository,
        seed_query: &str,
        candidates: &mut HashMap<String, CandidateScore>,
    ) -> Result<(), Status> {
        let terms = search_terms(seed_query);
        if terms.is_empty() {
            return Ok(());
        }
        let metas = repo
            .vault_list("memory/", Some(VaultEntryType::Memory))
            .map_err(repository_error)?;
        for meta in metas {
            let Some(node_id) = memory_path_to_node_id(&meta.path) else {
                continue;
            };
            let Some(entry) = repo.vault_retrieve(&meta.path).map_err(repository_error)? else {
                continue;
            };
            if !is_eligible_memory(&entry) {
                continue;
            }
            let body = String::from_utf8_lossy(&entry.content_bytes);
            let matched = body_match_fraction(&terms, &body);
            if matched > 0.0 {
                let base = BODY_MATCH_WEIGHT * matched;
                merge_candidate(candidates, &node_id, Some(meta.path), base, false);
            }
        }
        Ok(())
    }

    /// Seed query terms and graph-neighbor candidates from an intent.
    fn seed_from_intent(
        repo: &Repository,
        intent_id: &str,
        seed_terms: &mut Vec<String>,
        candidates: &mut HashMap<String, CandidateScore>,
    ) -> Result<(), Status> {
        let manifest = repo.vault_manifest().map_err(repository_error)?;
        let Some((resolved_id, summary)) = manifest
            .intents
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(intent_id))
        else {
            return Err(domain_status(
                ErrorCode::NotFound,
                format!("vault intent '{intent_id}' not found"),
            ));
        };

        seed_terms.push(summary.title.clone());

        // Legacy manifest entries may not carry `vault_path`; resolve the
        // path rather than querying the empty and meaningless KG node.
        let intent_path = repo
            .vault_intent_path(resolved_id)
            .map_err(repository_error)?
            .ok_or_else(|| {
                domain_status(
                    ErrorCode::NotFound,
                    format!("vault intent '{intent_id}' not found"),
                )
            })?;

        if let Some(entry) = repo
            .vault_retrieve(&intent_path)
            .map_err(repository_error)?
        {
            seed_terms.extend(frontmatter_labels(&entry.frontmatter_json));
            let body = String::from_utf8_lossy(&entry.content_bytes);
            let body = truncate_chars(body.trim(), INTENT_BODY_SEED_CHARS);
            if !body.is_empty() {
                seed_terms.push(body);
            }
        }

        let node_id = intent_path_to_node_id(&intent_path);
        // Intent -> referenced file/domain -> memory is commonly two hops.
        add_memory_neighbors(repo, &node_id, 2, candidates)?;
        Ok(())
    }

    /// Load candidate bodies from the vault, score, and rank them — the
    /// final ranked context items, truncated to `limit`.
    fn resolve_and_rank(
        repo: &Repository,
        candidates: HashMap<String, CandidateScore>,
        limit: usize,
        include_body: bool,
    ) -> Result<Vec<ContextItem>, Status> {
        let now = chrono::Utc::now();
        let mut items: Vec<ContextItem> = Vec::new();

        for (node_id, candidate) in candidates {
            let Some(path) = candidate
                .path
                .clone()
                .or_else(|| legacy_memory_node_id_to_path(&node_id))
            else {
                continue;
            };
            let Some(entry) = repo.vault_retrieve(&path).map_err(repository_error)? else {
                continue; // stale KG node — the entry no longer exists
            };

            if !is_eligible_memory(&entry) {
                continue;
            }
            let kind = frontmatter_kind(&entry.frontmatter_json);
            let Some(status) = frontmatter_status(&entry.frontmatter_json) else {
                continue;
            };

            let memory_id =
                frontmatter_memory_id(&entry.frontmatter_json).unwrap_or_else(|| node_id.clone());
            let name = frontmatter_name(&entry.frontmatter_json).unwrap_or_else(|| {
                node_id
                    .rsplit_once(':')
                    .map(|(_, n)| n)
                    .unwrap_or(&node_id)
                    .to_string()
            });
            let recency = recency_score(&entry.updated_at, now);
            let body = if include_body {
                String::from_utf8_lossy(&entry.content_bytes)
                    .trim()
                    .to_string()
            } else {
                String::new()
            };
            items.push(ContextItem {
                memory_id,
                kg_node_id: node_id,
                revision_hash: vault_entry_revision_hash(&entry),
                content_hash: atomic_core::types::Hash::from_bytes(entry.content_hash).to_base32(),
                path,
                name,
                kind,
                status,
                updated_at: entry.updated_at.clone(),
                introduced_by: entry.introduced_by,
                score: final_score(&candidate, recency),
                why_matched: why_matched(&candidate).to_string(),
                body: include_body.then_some(body),
                preview: None,
                recorded: entry.introduced_by != 0,
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

    /// Truncate bodies so their combined length fits `budget_chars`: an
    /// even allocation first, then unused capacity from short bodies goes
    /// to higher-ranked items instead of being discarded.
    fn apply_budget(items: &mut [ContextItem], budget_chars: usize) {
        if items.is_empty() {
            return;
        }
        let lengths: Vec<usize> = items
            .iter()
            .map(|item| item.body.as_deref().map_or(0, |body| body.chars().count()))
            .collect();
        let baseline = budget_chars / items.len();
        let mut allocations: Vec<usize> = lengths
            .iter()
            .map(|length| (*length).min(baseline))
            .collect();
        let mut remaining = budget_chars.saturating_sub(allocations.iter().sum());
        for (allocation, length) in allocations.iter_mut().zip(&lengths) {
            let extra = length.saturating_sub(*allocation).min(remaining);
            *allocation += extra;
            remaining -= extra;
            if remaining == 0 {
                break;
            }
        }

        for ((item, allocation), length) in items.iter_mut().zip(allocations).zip(lengths) {
            if allocation < length {
                let body = item.body.clone().unwrap_or_default();
                item.body = Some(truncate_chars(&body, allocation));
                item.truncated = true;
            }
        }
    }

    /// The gather→rank→budget pipeline for one context request: the ranked
    /// ContextItems the CLI renders (md, candidates, and JSON forms).
    pub fn context_items(
        repo: &Repository,
        query: &[String],
        intent: Option<&str>,
        files: &[String],
        limit: usize,
        budget_chars: usize,
        include_body: bool,
    ) -> Result<Vec<ContextItem>, Status> {
        let candidates = gather_candidates(repo, query, intent, files, limit)?;
        let mut items = resolve_and_rank(repo, candidates, limit, include_body)?;
        if include_body {
            apply_budget(&mut items, budget_chars);
            for item in &mut items {
                let preview = item.body.as_deref().map(preview).unwrap_or_default();
                item.preview = Some(preview);
            }
        } else {
            for item in &mut items {
                item.preview = None;
            }
        }
        Ok(items)
    }
}

pub struct VaultImpl {
    pub state: Arc<DaemonState>,
}

fn parse_frontmatter(
    entry: &atomic_core::pristine::VaultEntry,
) -> Result<Map<String, Value>, Status> {
    serde_json::from_str(&entry.frontmatter_json).map_err(|error| {
        domain_status(
            ErrorCode::Repository,
            format!("vault frontmatter is not valid JSON: {error}"),
        )
    })
}

fn body_of(entry: &atomic_core::pristine::VaultEntry) -> String {
    String::from_utf8_lossy(&entry.content_bytes).into_owned()
}

/// The sanitized id used for sidecar/vault attestation paths — mirrors the
/// CLI bridge's sanitizer (ASCII alphanumerics, `-`, `_`; everything else
/// becomes `_`).
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The normalized intent key (repo resolver first, uppercased fallback) —
/// mirrors the CLI bridge's `normalized_id`.
fn normalized_id(repo: &Repository, id: &str) -> String {
    repo.resolve_intent_key(id)
        .unwrap_or_else(|_| id.to_uppercase())
}

/// The fresh attested intent node, when a current attestation exists.
fn fresh_attested_intent(
    repo: &Repository,
    id: &str,
    frontmatter: &Map<String, Value>,
    body: &str,
) -> Option<CanonicalNode> {
    let vault_path = format!(
        "attestations/{}/attested.md",
        sanitize_id(&normalized_id(repo, id))
    );
    let entry = repo.vault_retrieve(&vault_path).ok()??;
    let node: CanonicalNode = serde_json::from_str(body_of(&entry).trim_end()).ok()?;
    let fresh = serde_json::from_str::<Value>(&entry.frontmatter_json)
        .ok()
        .and_then(|v| {
            v.get("sourceContentHash")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    if fresh.as_deref() == Some(source_content_hash(frontmatter, body).as_str()) {
        Some(node)
    } else {
        None
    }
}

/// The fresh attested memory node, when a current attestation exists.
fn fresh_attested_memory(
    repo: &Repository,
    id: &str,
    frontmatter: &Map<String, Value>,
    body: &str,
) -> Option<MemoryNode> {
    let vault_path = format!("attestations/memory/{}/attested.md", sanitize_id(id));
    let entry = repo.vault_retrieve(&vault_path).ok()??;
    let node: MemoryNode = serde_json::from_str(body_of(&entry).trim_end()).ok()?;
    let fresh = serde_json::from_str::<Value>(&entry.frontmatter_json)
        .ok()
        .and_then(|v| {
            v.get("sourceContentHash")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    if fresh.as_deref() == Some(source_content_hash(frontmatter, body).as_str()) {
        Some(node)
    } else {
        None
    }
}

/// The versioned-opaque entry bundle (schema "atomic.vault.entry.bundle.v1"):
/// the stored entry's full domain serialization plus the RAW attestation
/// sources, so the client runs its existing lift/classification code over
/// wire-carried inputs. `kind` selects the attestation scheme (the intent
/// and memory bridges' dual-read paths); `None` bundles the entry with no
/// attestation (the path-resolved `vault show` projection).
fn vault_entry_bundle(
    repo: &Repository,
    kind: Option<Kind>,
    id_or_path: &str,
    entry: &atomic_core::pristine::VaultEntry,
) -> Result<VersionedBytes, Status> {
    let attestation = match kind {
        Some(Kind::Intent) => intent_attestation_sources(repo, id_or_path),
        Some(Kind::Memory) => memory_attestation_sources(repo, id_or_path),
        _ => Value::Null,
    };
    let payload = serde_json::json!({
        "entry": entry,
        "attestation": attestation,
    });
    serde_json::to_vec(&payload)
        .map(|payload| VersionedBytes {
            schema: "atomic.vault.entry.bundle.v1".to_string(),
            payload,
        })
        .map_err(|error| Status::internal(error.to_string()))
}

/// The RAW intent attestation sources (the bridge's dual-read inputs):
/// the tracked vault entry (frontmatter + body bytes) and the legacy
/// sidecar candidate's text — the first EXISTING candidate (the
/// normalized-id path, then the raw-arg path a pre-upgrade build wrote).
/// Both ride the wire when both exist; the client's classification keeps
/// the bridge's precedence (tracked first, sidecar fallback).
fn intent_attestation_sources(repo: &Repository, id: &str) -> Value {
    let vpath = format!(
        "attestations/{}/attested.md",
        sanitize_id(&normalized_id(repo, id))
    );
    let tracked = repo.vault_retrieve(&vpath).ok().flatten().map(|entry| {
        serde_json::json!({
            "path": vpath,
            "frontmatter_json": entry.frontmatter_json,
            "content": entry.content_bytes,
        })
    });
    let normalized = repo
        .dot_dir()
        .join("canonical")
        .join("intents")
        .join(sanitize_id(&normalized_id(repo, id)))
        .join("attested.jsonld");
    let raw = repo
        .dot_dir()
        .join("canonical")
        .join("intents")
        .join(sanitize_id(id))
        .join("attested.jsonld");
    let sidecar = [normalized, raw]
        .into_iter()
        .find(|candidate| candidate.exists())
        .and_then(|candidate| {
            std::fs::read_to_string(&candidate).ok().map(|text| {
                serde_json::json!({
                    "path": candidate.display().to_string(),
                    "text": text,
                })
            })
        });
    serde_json::json!({ "tracked": tracked, "sidecar": sidecar })
}

/// The RAW memory attestation sources: the tracked
/// `attestations/memory/<id>/attested.md` entry and the single legacy
/// sidecar path's text.
fn memory_attestation_sources(repo: &Repository, id: &str) -> Value {
    let vpath = format!("attestations/memory/{}/attested.md", sanitize_id(id));
    let tracked = repo.vault_retrieve(&vpath).ok().flatten().map(|entry| {
        serde_json::json!({
            "path": vpath,
            "frontmatter_json": entry.frontmatter_json,
            "content": entry.content_bytes,
        })
    });
    let sidecar_path = repo
        .dot_dir()
        .join("canonical")
        .join("memory")
        .join(sanitize_id(id))
        .join("attested.jsonld");
    let sidecar = if sidecar_path.exists() {
        std::fs::read_to_string(&sidecar_path).ok().map(|text| {
            serde_json::json!({
                "path": sidecar_path.display().to_string(),
                "text": text,
            })
        })
    } else {
        None
    };
    serde_json::json!({ "tracked": tracked, "sidecar": sidecar })
}

fn memory_is_index_scaffold(repo: &Repository, path: &str) -> bool {
    let Ok(Some(entry)) = repo.vault_retrieve(path) else {
        return false;
    };
    serde_json::from_str::<Value>(&entry.frontmatter_json)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some("index")
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// Listing-surface domain logic — the attestation-aware listings
//
// The per-entry classification the `intent list`/`memory list` columns and
// JSON carry: the manifest kind tag, the tracked attestation's fresh/stale
// state (with the legacy-sidecar dual-read), and the
// DID-match-then-verify rule. Ported from the CLI bodies so the ranking
// recipe lives in exactly one place (the handler); the CLI renders.
// ---------------------------------------------------------------------------

/// The verifying identity resolved ONCE for a whole listing: its public
/// key and DID. `None` soft-fails (no identity resolvable — every
/// `verifies` cell reads "na"); a NAMED-but-missing identity is a hard
/// error, matching the verify verbs.
struct ListingVerifier {
    public_key: atomic_identity::keypair::PublicKey,
    did: String,
}

fn resolve_listing_verifier(name: &Option<String>) -> Result<Option<ListingVerifier>, Status> {
    let Ok(store) = IdentityStore::open_default() else {
        return Ok(None);
    };
    let identity = match name {
        Some(name) => Some(store.load_by_name(name).map_err(|_| {
            domain_status(ErrorCode::NotFound, format!("identity '{name}' not found"))
        })?),
        None => store.get_default().ok().flatten(),
    };
    Ok(identity.map(|identity| ListingVerifier {
        did: did_for_public_key(&identity.public_key),
        public_key: identity.public_key.clone(),
    }))
}

/// The DID-match-then-verify rule (the lists' `verifies` column):
/// "na" when there is no fresh attestation, no resolvable identity, or a
/// different signer; "yes"/"no" only for a same-signer fresh node. The
/// same rule the CLI bodies apply, ported so the handler owns it.
fn verifies_token<N>(
    node: Option<&N>,
    attributed_to: Option<&str>,
    verifier: Option<&ListingVerifier>,
    verify: impl FnOnce(&N, &atomic_identity::keypair::PublicKey) -> bool,
) -> &'static str {
    let (Some(node), Some(verifier)) = (node, verifier) else {
        return "na";
    };
    // DID pre-check FIRST: a different (or absent) signer is "na", not
    // "no" (a merely-unresolvable signer is NOT a failure).
    if attributed_to != Some(verifier.did.as_str()) {
        return "na";
    }
    // Same signer ⇒ the only path to "no" is a real hash/signature failure.
    if verify(node, &verifier.public_key) {
        "yes"
    } else {
        "no"
    }
}

/// One loaded attestation artifact, classified against the current source
/// (the fresh/stale/none trichotomy the listings render).
enum LoadedAttestation<N> {
    Fresh(N),
    Stale(N),
    None,
}

/// The fresh/stale classification: the recorded source-content anchor
/// versus the current source hash (the same rule the CLI bridge applies).
fn classify_artifact<N>(
    node: N,
    recorded: Option<String>,
    current: String,
) -> LoadedAttestation<N> {
    if recorded.as_deref() == Some(current.as_str()) {
        LoadedAttestation::Fresh(node)
    } else {
        LoadedAttestation::Stale(node)
    }
}

/// The `sourceContentHash` anchor off a tracked attestation entry's
/// frontmatter.
fn tracked_source_anchor(entry: &atomic_core::pristine::VaultEntry) -> Option<String> {
    serde_json::from_str::<Value>(&entry.frontmatter_json)
        .ok()
        .and_then(|v| {
            v.get("sourceContentHash")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

/// An intent's attestation, loaded through the same dual-read the CLI
/// bridge performs: the tracked vault entry first (its body parses to a
/// `CanonicalNode`), then the legacy sidecar candidates (the normalized
/// id path, then the raw-arg path a pre-upgrade build wrote).
fn intent_list_attestation(
    repo: &Repository,
    id: &str,
    frontmatter: &Map<String, Value>,
    body: &str,
) -> LoadedAttestation<CanonicalNode> {
    let vpath = format!(
        "attestations/{}/attested.md",
        sanitize_id(&normalized_id(repo, id))
    );
    if let Ok(Some(entry)) = repo.vault_retrieve(&vpath) {
        if let Ok(node) = serde_json::from_str::<CanonicalNode>(body_of(&entry).trim_end()) {
            return classify_artifact(
                node,
                tracked_source_anchor(&entry),
                source_content_hash(frontmatter, body),
            );
        }
        // A malformed tracked entry falls through to the sidecar.
    }
    let normalized = repo
        .dot_dir()
        .join("canonical")
        .join("intents")
        .join(sanitize_id(&normalized_id(repo, id)))
        .join("attested.jsonld");
    let raw = repo
        .dot_dir()
        .join("canonical")
        .join("intents")
        .join(sanitize_id(id))
        .join("attested.jsonld");
    for path in [normalized, raw] {
        if !path.exists() {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(artifact) = serde_json::from_str::<Value>(&text) {
                if let Ok(node) = serde_json::from_value::<CanonicalNode>(
                    artifact.get("node").cloned().unwrap_or(Value::Null),
                ) {
                    let recorded = artifact
                        .get("source")
                        .and_then(|s| s.get("sourceContentHash"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    return classify_artifact(
                        node,
                        recorded,
                        source_content_hash(frontmatter, body),
                    );
                }
            }
            // The CLI bridge warns and treats an unreadable sidecar as
            // none; the listing degrades the row the same way.
            return LoadedAttestation::None;
        }
    }
    LoadedAttestation::None
}

/// A memory's attestation, loaded through the memory bridge's dual-read:
/// the tracked `attestations/memory/<id>/attested.md` entry first, then
/// the single legacy sidecar path.
fn memory_list_attestation(
    repo: &Repository,
    id: &str,
    frontmatter: &Map<String, Value>,
    body: &str,
) -> LoadedAttestation<MemoryNode> {
    let vpath = format!("attestations/memory/{}/attested.md", sanitize_id(id));
    if let Ok(Some(entry)) = repo.vault_retrieve(&vpath) {
        if let Ok(node) = serde_json::from_str::<MemoryNode>(body_of(&entry).trim_end()) {
            return classify_artifact(
                node,
                tracked_source_anchor(&entry),
                source_content_hash(frontmatter, body),
            );
        }
    }
    let path = repo
        .dot_dir()
        .join("canonical")
        .join("memory")
        .join(sanitize_id(id))
        .join("attested.jsonld");
    if path.exists() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(artifact) = serde_json::from_str::<Value>(&text) {
                if let Ok(node) = serde_json::from_value::<MemoryNode>(
                    artifact.get("node").cloned().unwrap_or(Value::Null),
                ) {
                    let recorded = artifact
                        .get("source")
                        .and_then(|s| s.get("sourceContentHash"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    return classify_artifact(
                        node,
                        recorded,
                        source_content_hash(frontmatter, body),
                    );
                }
            }
            return LoadedAttestation::None;
        }
    }
    LoadedAttestation::None
}

/// The local vault-init body's recursive tracker: add every file under
/// the vault directory (relative to the repository root).
fn add_vault_files_recursive(
    repo: &atomic_repository::Repository,
    working_copy: atomic_core::types::WorkingCopyId,
    dir: &std::path::Path,
) -> Result<(), Status> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                add_vault_files_recursive(repo, working_copy, &path)?;
            } else if path.is_file() {
                if let Ok(relative) = path.strip_prefix(repo.root()) {
                    let _ = repo.add(
                        working_copy,
                        relative,
                        atomic_repository::TrackingOptions::default(),
                    );
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn response_meta(meta: &Option<RequestMeta>) -> Option<ResponseMeta> {
    meta.as_ref().map(|meta| ResponseMeta {
        request_id: meta.request_id.clone(),
        replayed: false,
        snapshot: None,
    })
}

#[tonic::async_trait]
impl vault_service_server::VaultService for VaultImpl {
    async fn create_vault_entity(
        &self,
        request: Request<CreateVaultEntityRequest>,
    ) -> Result<Response<CreateVaultEntityResponse>, Status> {
        let request = request.into_inner();
        let kind = Kind::try_from(request.kind)
            .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown vault entity kind"))?;
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("CreateVaultEntity", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let response = match kind {
            Kind::Intent => {
                let title = request.title.clone();
                let handle_for_task = handle.clone();
                let result = tokio::task::spawn_blocking({
                    let handle = handle_for_task;
                    move || {
                        let repo = handle.repository()?;
                        let created = repo
                            .vault_intent_create(IntentCreateOptions {
                                title: title.clone(),
                                priority: None,
                                assignee: None,
                                labels: Vec::new(),
                                session_id: None,
                                turn_id: None,
                                kind: None,
                            })
                            .map_err(repository_error)?;
                        // The CLI's `intent new` overwrites the legacy
                        // positional scaffold with the directive scaffold
                        // through the same update path — the daemon must
                        // produce the identical artifact.
                        let scaffold =
                            atomic_repository::FEATURE_SCAFFOLD.replace("{id}", &created.uid);
                        repo.vault_intent_update(
                            &created.id,
                            IntentUpdateOptions {
                                content: Some(scaffold),
                                force: true,
                                ..Default::default()
                            },
                        )
                        .map_err(repository_error)?;
                        Ok::<_, Status>(created)
                    }
                })
                .await
                .map_err(|e| Status::internal(e.to_string()))??;
                CreateVaultEntityResponse {
                    entry: Some(VaultEntry {
                        kind: Kind::Intent as i32,
                        id: result.id.clone(),
                        title: request.title.clone(),
                        status: Some(VaultEntityStatus::Backlog as i32),
                        body: request.body.clone(),
                        created_at: None,
                        updated_at: None,
                        linked: Vec::new(),
                        vault_path: Some(format!(".vault/{}", result.intent_file)),
                        priority: Some("medium".to_string()),
                        ..Default::default()
                    }),
                    meta: response_meta(&meta),
                }
            }
            Kind::Goal => {
                // `vault goal start` — GoalStartOptions' fields ride the
                // request (add-only); the created goal's dir/file and the
                // listing columns ride the response entry.
                let handle_for_task = handle.clone();
                let title = request.title.clone();
                let developer = request.developer.clone();
                let intent = request.intent.clone();
                let model = request.model.clone();
                let meta_for_task = meta.clone();
                let result = tokio::task::spawn_blocking({
                    let handle = handle_for_task;
                    move || {
                        let repo = handle.repository()?;
                        repo.vault_goal_start(atomic_repository::GoalStartOptions {
                            name: (!title.is_empty()).then_some(title.clone()),
                            developer,
                            intent,
                            model,
                        })
                        .map_err(repository_error)
                    }
                })
                .await
                .map_err(|e| Status::internal(e.to_string()))??;
                CreateVaultEntityResponse {
                    entry: Some(VaultEntry {
                        kind: Kind::Goal as i32,
                        id: result.name.clone(),
                        title: String::new(),
                        status: None,
                        body: None,
                        created_at: None,
                        updated_at: None,
                        linked: request.intent.clone().into_iter().collect(),
                        vault_path: None,
                        priority: None,
                        developer: request.developer.clone(),
                        goal_dir: Some(result.goal_dir.clone()),
                        goal_file: Some(result.goal_file.clone()),
                        ..Default::default()
                    }),
                    meta: response_meta(&meta_for_task),
                }
            }
            Kind::Memory => {
                let memory_kind = request
                    .memory_kind
                    .clone()
                    .unwrap_or_else(|| "context".to_string());
                let text = request.body.clone().unwrap_or_default();
                if text.trim().is_empty() {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "memory text must not be empty",
                    ));
                }
                let derived_from = request.derived_from.clone();
                let about = request.about.clone();
                let id = ulid::Ulid::new().to_string().to_lowercase();
                let vault_path = format!("memory/{id}.md");
                let body = format!("{}\n", text.trim());
                let body_for_store = body.clone();
                let handle_for_task = handle.clone();
                let response = tokio::task::spawn_blocking({
                    let handle = handle_for_task;
                    let vault_path = vault_path.clone();
                    let id = id.clone();
                    let body = body_for_store;
                    move || {
                        let repo = handle.repository()?;
                        let mut frontmatter = Map::new();
                        frontmatter.insert("uid".into(), Value::String(id));
                        frontmatter.insert("memoryKind".into(), Value::String(memory_kind));
                        frontmatter.insert("status".into(), Value::String("active".into()));
                        frontmatter.insert("createdAt".into(), Value::String(now_rfc3339()));
                        if !about.is_empty() {
                            frontmatter.insert(
                                "about".into(),
                                Value::Array(
                                    about.iter().map(|a| Value::String(a.clone())).collect(),
                                ),
                            );
                        }
                        if !derived_from.is_empty() {
                            frontmatter.insert(
                                "derivedFrom".into(),
                                Value::Array(
                                    derived_from
                                        .iter()
                                        .map(|d| Value::String(d.clone()))
                                        .collect(),
                                ),
                            );
                        }
                        let frontmatter_json = serde_json::to_string(&frontmatter)
                            .map_err(|e| Status::internal(e.to_string()))?;
                        repo.vault_store(
                            &vault_path,
                            VaultEntryType::Memory,
                            body.into_bytes(),
                            frontmatter_json,
                        )
                        .map_err(repository_error)?;
                        repo.vault_materialize(&vault_path)
                            .map_err(repository_error)?;
                        Ok::<_, Status>(())
                    }
                })
                .await
                .map_err(|e| Status::internal(e.to_string()))??;
                let _ = response;
                CreateVaultEntityResponse {
                    entry: Some(VaultEntry {
                        kind: Kind::Memory as i32,
                        id: id.clone(),
                        title: id.clone(),
                        status: Some(VaultEntityStatus::Active as i32),
                        body: Some(body),
                        created_at: None,
                        updated_at: None,
                        linked: Vec::new(),
                        vault_path: Some(format!(".vault/{vault_path}")),
                        priority: None,
                        ..Default::default()
                    }),
                    meta: response_meta(&meta),
                }
            }
            other => {
                return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    format!("vault entity kind {other:?} lands with its slice (unimplemented)"),
                ))
            }
        };
        Ok(Response::new(response))
    }

    async fn update_vault_entity(
        &self,
        request: Request<UpdateVaultEntityRequest>,
    ) -> Result<Response<UpdateVaultEntityResponse>, Status> {
        let request = request.into_inner();
        let kind = Kind::try_from(request.kind)
            .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown vault entity kind"))?;
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("UpdateVaultEntity", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let update = request
            .update
            .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "update kind is required"))?;
        let id = request.id.clone();
        let response = tokio::task::spawn_blocking(move || match kind {
            Kind::Intent => {
                let repo = handle.repository()?;
                let mut options = IntentUpdateOptions::default();
                match update {
                    UpdateKind::Status(status) => {
                        let proto = VaultEntityStatus::try_from(status).map_err(|_| {
                            domain_status(ErrorCode::InvalidArgument, "unknown entity status")
                        })?;
                        let status = intent_status_from_proto(proto).ok_or_else(|| {
                            domain_status(
                                ErrorCode::InvalidArgument,
                                "status is not part of the intent lifecycle",
                            )
                        })?;
                        options.status = Some(status.to_string());
                    }
                    UpdateKind::Body(body) => {
                        options.content = Some(body);
                    }
                    // The fully composed update: every field in ONE
                    // request, applied atomically by the same domain
                    // update the CLI's multi-flag form performs.
                    UpdateKind::Fields(fields) => {
                        options.status = fields.status;
                        options.assignee = fields.assignee;
                        options.priority = fields.priority;
                        options.title = fields.title;
                        options.reason = fields.reason;
                        options.informed_by =
                            (!fields.informed_by.is_empty()).then_some(fields.informed_by);
                        options.content = fields.body;
                        options.force = fields.force;
                    }
                    _ => {
                        return Err(domain_status(
                            ErrorCode::InvalidArgument,
                            "update kind is not part of the intent lifecycle",
                        ));
                    }
                }
                let info = repo
                    .vault_intent_update(&id, options)
                    .map_err(repository_error)?;
                Ok::<_, Status>(UpdateVaultEntityResponse {
                    entry: Some(VaultEntry {
                        kind: Kind::Intent as i32,
                        id: info.id.clone(),
                        title: info.title.clone(),
                        status: intent_status_to_proto(&info.status).map(|s| s as i32),
                        body: None,
                        created_at: None,
                        updated_at: None,
                        linked: Vec::new(),
                        vault_path: None,
                        priority: Some(info.priority.clone()),
                        assignee: info.assignee.clone(),
                        status_label: Some(info.status.clone()),
                        ..Default::default()
                    }),
                    write_path: None,
                    write_hash: None,
                    stop_status: None,
                    paths_removed: None,
                    meta: response_meta(&meta),
                })
            }
            Kind::Memory => match update {
                UpdateKind::Body(body) => {
                    let repo = handle.repository()?;
                    let vault_path = format!("memory/{id}.md");
                    let entry = repo
                        .vault_retrieve(&vault_path)
                        .map_err(repository_error)?
                        .ok_or_else(|| domain_status(ErrorCode::NotFound, "memory not found"))?;
                    let mut frontmatter = parse_frontmatter(&entry)?;
                    frontmatter.insert("updatedAt".into(), Value::String(now_rfc3339()));
                    let frontmatter_json = serde_json::to_string(&frontmatter)
                        .map_err(|e| Status::internal(e.to_string()))?;
                    repo.vault_store(
                        &vault_path,
                        VaultEntryType::Memory,
                        body.into_bytes(),
                        frontmatter_json,
                    )
                    .map_err(repository_error)?;
                    repo.vault_materialize(&vault_path)
                        .map_err(repository_error)?;
                    Ok(UpdateVaultEntityResponse {
                        entry: Some(VaultEntry {
                            kind: Kind::Memory as i32,
                            id: id.clone(),
                            title: id.clone(),
                            status: None,
                            body: None,
                            created_at: None,
                            updated_at: None,
                            linked: Vec::new(),
                            vault_path: Some(format!(".vault/{vault_path}")),
                            priority: None,
                            ..Default::default()
                        }),
                        write_path: None,
                        write_hash: None,
                        stop_status: None,
                        paths_removed: None,
                        meta: response_meta(&meta),
                    })
                }
                UpdateKind::MemoryWrite(memory_write) => {
                    // `atomic memory write` — the raw store + materialize
                    // (create-or-replace at the named vault path), with the
                    // content hash riding the response.
                    let repo = handle.repository()?;
                    let hash = repo
                        .vault_store(
                            &memory_write.path,
                            VaultEntryType::Memory,
                            memory_write.content.clone(),
                            memory_write.frontmatter_json.clone(),
                        )
                        .map_err(repository_error)?;
                    repo.vault_materialize(&memory_write.path)
                        .map_err(repository_error)?;
                    Ok(UpdateVaultEntityResponse {
                        entry: None,
                        write_path: Some(memory_write.path.clone()),
                        write_hash: Some(super::convert::hash_proto(&hash)),
                        stop_status: None,
                        paths_removed: None,
                        meta: response_meta(&meta),
                    })
                }
                UpdateKind::Status(_)
                | UpdateKind::Fields(_)
                | UpdateKind::GoalStop(_)
                | UpdateKind::GoalResume(_) => Err(domain_status(
                    ErrorCode::InvalidArgument,
                    "memory status transitions are new revisions, not updates",
                )),
            },
            Kind::Goal => match update {
                // `vault goal stop` — the goal to stop (empty = the most
                // recent active goal) + the promote/discard options.
                UpdateKind::GoalStop(goal_stop) => {
                    let repo = handle.repository()?;
                    let goal_name = match &goal_stop.goal {
                        Some(name) => name.clone(),
                        None => repo
                            .vault_goal_list(Some("active"))
                            .map_err(repository_error)?
                            .first()
                            .map(|summary| summary.name.clone())
                            .ok_or_else(|| {
                                domain_status(
                                    ErrorCode::InvalidArgument,
                                    "No active goal found. Specify a goal name.",
                                )
                            })?,
                    };
                    let result = repo
                        .vault_goal_stop(
                            &goal_name,
                            atomic_repository::GoalStopOptions {
                                promote: goal_stop.promote,
                                discard: goal_stop.discard,
                            },
                        )
                        .map_err(repository_error)?;
                    Ok(UpdateVaultEntityResponse {
                        entry: Some(VaultEntry {
                            kind: Kind::Goal as i32,
                            id: result.name.clone(),
                            title: String::new(),
                            status: None,
                            body: None,
                            created_at: None,
                            updated_at: None,
                            linked: Vec::new(),
                            vault_path: None,
                            priority: None,
                            status_label: Some(result.status.clone()),
                            ..Default::default()
                        }),
                        write_path: None,
                        write_hash: None,
                        stop_status: Some(result.status.clone()),
                        paths_removed: Some(result.paths_removed as u32),
                        meta: response_meta(&meta),
                    })
                }
                // `vault goal resume`.
                UpdateKind::GoalResume(goal_resume) => {
                    let repo = handle.repository()?;
                    let info = repo
                        .vault_goal_resume(&goal_resume.goal)
                        .map_err(repository_error)?;
                    Ok(UpdateVaultEntityResponse {
                        entry: Some(VaultEntry {
                            kind: Kind::Goal as i32,
                            id: info.name.clone(),
                            title: info.developer.clone(),
                            status: None,
                            body: None,
                            created_at: None,
                            updated_at: None,
                            linked: info.intent.clone().into_iter().collect(),
                            vault_path: None,
                            priority: None,
                            developer: Some(info.developer.clone()),
                            status_label: Some(info.status.clone()),
                            started_at: Some(info.started_at.clone()),
                            turns: Some(info.turns),
                            ..Default::default()
                        }),
                        write_path: None,
                        write_hash: None,
                        stop_status: None,
                        paths_removed: None,
                        meta: response_meta(&meta),
                    })
                }
                _ => Err(domain_status(
                    ErrorCode::InvalidArgument,
                    "update kind is not part of the goal lifecycle",
                )),
            },
            other => Err(domain_status(
                ErrorCode::InvalidArgument,
                format!("vault entity kind {other:?} lands with its slice (unimplemented)"),
            )),
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn get_vault_entry(
        &self,
        request: Request<GetVaultEntryRequest>,
    ) -> Result<Response<GetVaultEntryResponse>, Status> {
        let request = request.into_inner();
        let kind = Kind::try_from(request.kind)
            .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown vault entity kind"))?;
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("GetVaultEntry", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let id = request.id.clone();
        let path = request.path.clone();
        let include_bundle = request.include_bundle;
        let response = tokio::task::spawn_blocking(move || {
            // Path-first resolution (add-only): `vault show <path>` names
            // ANY vault entry by its vault-relative path — no kind
            // inference. The bundle carries the stored entry (plus no
            // attestation: the vault-show projection never consults one).
            if let Some(path) = path.as_deref().filter(|path| !path.is_empty()) {
                // A query: read-only open with the read-concurrency wait.
                let repo = handle.repository_readonly()?;
                let entry = repo
                    .vault_retrieve(path)
                    .map_err(repository_error)?
                    .ok_or_else(|| {
                        domain_status(
                            ErrorCode::NotFound,
                            format!("vault entry '{path}' not found"),
                        )
                    })?;
                let bundle = include_bundle
                    .then(|| vault_entry_bundle(&repo, None, path, &entry))
                    .transpose()?;
                return Ok::<_, Status>(GetVaultEntryResponse {
                    entry: Some(VaultEntry {
                        kind: Kind::Unspecified as i32,
                        id: path.to_string(),
                        title: String::new(),
                        status: None,
                        body: Some(body_of(&entry)),
                        created_at: None,
                        updated_at: None,
                        linked: Vec::new(),
                        vault_path: Some(path.to_string()),
                        priority: None,
                        entry_type: Some(entry.entry_type.to_string()),
                        updated_at_label: Some(entry.updated_at.clone()),
                        ..Default::default()
                    }),
                    snapshot: None,
                    entry_bundle: bundle,
                });
            }
            match kind {
                Kind::Intent => {
                    // A query: read-only open with the read-concurrency wait.
                    let repo = handle.repository_readonly()?;
                    let entry = repo.vault_intent_show(&id).map_err(repository_error)?;
                    let frontmatter = parse_frontmatter(&entry)?;
                    let title = frontmatter
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let status = frontmatter
                        .get("status")
                        .and_then(Value::as_str)
                        .and_then(intent_status_to_proto)
                        .map(|s| s as i32);
                    let bundle = include_bundle
                        .then(|| vault_entry_bundle(&repo, Some(Kind::Intent), &id, &entry))
                        .transpose()?;
                    Ok(GetVaultEntryResponse {
                        entry: Some(VaultEntry {
                            kind: Kind::Intent as i32,
                            id: normalized_id(&repo, &id),
                            title,
                            status,
                            body: Some(body_of(&entry)),
                            created_at: None,
                            updated_at: None,
                            linked: Vec::new(),
                            vault_path: None,
                            priority: None,
                            ..Default::default()
                        }),
                        snapshot: None,
                        entry_bundle: bundle,
                    })
                }
                Kind::Memory => {
                    // A query: read-only open with the read-concurrency wait.
                    let repo = handle.repository_readonly()?;
                    // The memory id/path resolver (no manifest lookup, no
                    // case-folding): a bare id, a `memory/<id>` path, an
                    // `<id>.md` name, or a full `memory/<id>.md` path all
                    // resolve to `memory/<id>.md`.
                    let without_prefix = id.strip_prefix("memory/").unwrap_or(&id);
                    let stem = without_prefix.strip_suffix(".md").unwrap_or(without_prefix);
                    let vault_path = format!("memory/{stem}.md");
                    let entry = repo
                        .vault_retrieve(&vault_path)
                        .map_err(repository_error)?
                        .ok_or_else(|| domain_status(ErrorCode::NotFound, "memory not found"))?;
                    let bundle = include_bundle
                        .then(|| vault_entry_bundle(&repo, Some(Kind::Memory), stem, &entry))
                        .transpose()?;
                    Ok(GetVaultEntryResponse {
                        entry: Some(VaultEntry {
                            kind: Kind::Memory as i32,
                            id: stem.to_string(),
                            title: stem.to_string(),
                            status: None,
                            body: Some(body_of(&entry)),
                            created_at: None,
                            updated_at: None,
                            linked: Vec::new(),
                            vault_path: Some(format!(".vault/{vault_path}")),
                            priority: None,
                            ..Default::default()
                        }),
                        snapshot: None,
                        entry_bundle: bundle,
                    })
                }
                Kind::Goal => {
                    // `atomic vault goal show <goal>` — the stored goal.md
                    // entry (content + frontmatter ride the bundle).
                    let repo = handle.repository_readonly()?;
                    let entry = repo.vault_goal_show(&id).map_err(repository_error)?;
                    let bundle = include_bundle
                        .then(|| vault_entry_bundle(&repo, Some(Kind::Goal), &id, &entry))
                        .transpose()?;
                    Ok(GetVaultEntryResponse {
                        entry: Some(VaultEntry {
                            kind: Kind::Goal as i32,
                            id: id.clone(),
                            title: String::new(),
                            status: None,
                            body: Some(body_of(&entry)),
                            created_at: None,
                            updated_at: None,
                            linked: Vec::new(),
                            vault_path: None,
                            priority: None,
                            entry_type: Some(entry.entry_type.to_string()),
                            updated_at_label: Some(entry.updated_at.clone()),
                            ..Default::default()
                        }),
                        snapshot: None,
                        entry_bundle: bundle,
                    })
                }
                other => Err(domain_status(
                    ErrorCode::InvalidArgument,
                    format!("vault entity kind {other:?} lands with its slice (unimplemented)"),
                )),
            }
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn list_vault_entries(
        &self,
        request: Request<ListVaultEntriesRequest>,
    ) -> Result<Response<ListVaultEntriesResponse>, Status> {
        let request = request.into_inner();
        // kind absent → the whole-vault listing (`vault list`: every entry
        // with its type/size/date columns, prefix- and type-filtered).
        let kind = request.kind.and_then(|k| Kind::try_from(k).ok());
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ListVaultEntries", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let identity = request.identity;
        let path_prefix = request.path_prefix.clone().unwrap_or_default();
        let entry_type = request.entry_type.clone();
        let status_filter = request.status_filter.clone();
        let tool_result_previews = request.tool_result_previews;
        let limit = request
            .budget
            .and_then(|budget| budget.max_items)
            .map(|max| max as usize);
        let response = tokio::task::spawn_blocking(move || {
            if tool_result_previews {
                // `atomic vault summaries` — the ToolResult entries under the
                // prefix, each with its filename-stem id and a 200-char content
                // preview (the local body's exact enumeration + preview).
                let repo = handle.repository_readonly()?;
                let entries = repo
                    .vault_list(&path_prefix, Some(VaultEntryType::ToolResult))
                    .map_err(repository_error)?;
                let mut rows = Vec::with_capacity(entries.len());
                for meta in &entries {
                    let filename = meta.path.rsplit('/').next().unwrap_or(&meta.path);
                    let id = filename.strip_suffix(".md").unwrap_or(filename);
                    if let Ok(Some(entry)) = repo.vault_retrieve(&meta.path) {
                        let content = String::from_utf8_lossy(&entry.content_bytes);
                        let preview: String = content.chars().take(200).collect();
                        rows.push(VaultEntry {
                            kind: Kind::Unspecified as i32,
                            id: id.to_string(),
                            title: String::new(),
                            status: None,
                            body: Some(preview),
                            created_at: None,
                            updated_at: None,
                            linked: Vec::new(),
                            vault_path: Some(meta.path.clone()),
                            priority: None,
                            entry_type: Some("tool_result".to_string()),
                            ..Default::default()
                        });
                    }
                }
                return Ok::<_, Status>(ListVaultEntriesResponse {
                    entries: rows,
                    next_cursor: None,
                    snapshot: None,
                });
            }
            match kind {
                Some(Kind::Intent) => {
                    // A query: read-only open with the read-concurrency wait.
                    let repo = handle.repository_readonly()?;
                    // The verifying identity resolved ONCE (soft-fail to "no
                    // identity"; a named-but-missing identity is a hard error).
                    let verifier = resolve_listing_verifier(&identity)?;
                    let manifest = repo.vault_manifest().map_err(repository_error)?;
                    let infos = repo.vault_intent_list(None).map_err(repository_error)?;
                    let entries = infos
                        .into_iter()
                        .map(|info| {
                            // The manifest kind tag (a manifest read, never a
                            // lift); a row missing from the manifest degrades
                            // to the default "feature".
                            let manifest_kind = manifest
                                .intents
                                .get(&info.id)
                                .map(|summary| summary.kind.clone())
                                .unwrap_or_else(|| "feature".to_string());
                            // The attestation columns: a read/attestation
                            // failure for a SINGLE intent degrades that row's
                            // cells ("none"/"na"), never the whole list.
                            let (attested, verifies) = (|| {
                                let entry = repo.vault_intent_show(&info.id).ok()?;
                                let frontmatter = parse_frontmatter(&entry).ok()?;
                                let body = body_of(&entry);
                                match intent_list_attestation(&repo, &info.id, &frontmatter, &body)
                                {
                                    LoadedAttestation::Fresh(node) => Some((
                                        "fresh",
                                        verifies_token(
                                            Some(&node),
                                            node.attributed_to.as_deref(),
                                            verifier.as_ref(),
                                            |node, public_key| verify(node, public_key).is_ok(),
                                        ),
                                    )),
                                    LoadedAttestation::Stale(_) => Some(("stale", "na")),
                                    LoadedAttestation::None => Some(("none", "na")),
                                }
                            })()
                            .unwrap_or(("none", "na"));
                            VaultEntry {
                                kind: Kind::Intent as i32,
                                id: info.id.clone(),
                                title: info.title.clone(),
                                status: intent_status_to_proto(&info.status).map(|s| s as i32),
                                body: None, // listings are summaries; bodies stay unloaded
                                created_at: None,
                                updated_at: None,
                                linked: Vec::new(),
                                vault_path: None,
                                priority: Some(info.priority.clone()),
                                status_label: Some(info.status.clone()),
                                manifest_kind: Some(manifest_kind),
                                attested: Some(attested.to_string()),
                                verifies: Some(verifies.to_string()),
                                ..Default::default()
                            }
                        })
                        .collect();
                    Ok::<_, Status>(ListVaultEntriesResponse {
                        entries,
                        next_cursor: None,
                        snapshot: None,
                    })
                }
                Some(Kind::Memory) => {
                    // A query: read-only open with the read-concurrency wait.
                    let repo = handle.repository_readonly()?;
                    // The verifying identity resolved ONCE — the `verifies`
                    // column's resolver (the same soft/hard-fail rule).
                    let verifier = resolve_listing_verifier(&identity)?;
                    // The SAME enumeration the local body performs: the
                    // `memory/` prefix, never attestation entries, never the
                    // `MEMORY.md` index scaffold, most-recent first with the
                    // id as the stable tiebreaker, truncated to the budget.
                    let metas = repo.vault_list("memory/", None).map_err(repository_error)?;
                    let mut items: Vec<(String, String)> = metas
                        .iter()
                        .filter(|meta| !meta.path.starts_with("attestations/"))
                        .filter(|meta| !memory_is_index_scaffold(&repo, &meta.path))
                        .map(|meta| {
                            let id = meta
                                .path
                                .trim_end_matches(".md")
                                .trim_start_matches("memory/")
                                .to_string();
                            (id, meta.updated_at.clone())
                        })
                        .collect();
                    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                    if let Some(limit) = limit {
                        items.truncate(limit);
                    }
                    let entries = items
                        .iter()
                        .map(|(id, _)| {
                            // kind/status/about degrade to "–"/0 when a single
                            // memory cannot be read or lifted — never the list.
                            let degraded = (
                                "\u{2013}".to_string(),
                                "\u{2013}".to_string(),
                                0u32,
                                "none",
                                "na",
                            );
                            let (memory_kind, memory_status, about, attested, verifies) = (|| {
                                let path = format!("memory/{id}.md");
                                let entry = repo.vault_retrieve(&path).ok()??;
                                let frontmatter = parse_frontmatter(&entry).ok()?;
                                let body = body_of(&entry);
                                match memory_list_attestation(&repo, id, &frontmatter, &body) {
                                    LoadedAttestation::Fresh(node) => {
                                        // The fresh node drives the display
                                        // columns; the DID-match-then-verify
                                        // rule drives `verifies`.
                                        let verifies = verifies_token(
                                            Some(&node),
                                            node.attributed_to.as_deref(),
                                            verifier.as_ref(),
                                            |node, public_key| {
                                                verify_memory(node, public_key).is_ok()
                                            },
                                        );
                                        Some((
                                            node.memory_kind.clone(),
                                            node.status.clone(),
                                            node.about.len() as u32,
                                            "fresh",
                                            verifies,
                                        ))
                                    }
                                    other => {
                                        // Stale/None: the columns come from the
                                        // CURRENT source (the lift); the drift
                                        // is still surfaced by the attested
                                        // column.
                                        let token = match other {
                                            LoadedAttestation::Stale(_) => "stale",
                                            _ => "none",
                                        };
                                        match lift_memory(&frontmatter, &body) {
                                            Ok(node) => Some((
                                                node.memory_kind,
                                                node.status,
                                                node.about.len() as u32,
                                                token,
                                                "na",
                                            )),
                                            Err(_) => {
                                                let fm = |key: &str| {
                                                    frontmatter
                                                        .get(key)
                                                        .and_then(Value::as_str)
                                                        .map(str::to_owned)
                                                        .unwrap_or_else(|| "\u{2013}".to_string())
                                                };
                                                Some((
                                                    fm("memoryKind"),
                                                    fm("status"),
                                                    0,
                                                    token,
                                                    "na",
                                                ))
                                            }
                                        }
                                    }
                                }
                            })(
                            )
                            .unwrap_or(degraded);
                            VaultEntry {
                                kind: Kind::Memory as i32,
                                id: id.clone(),
                                title: id.clone(),
                                status: None,
                                body: None,
                                created_at: None,
                                updated_at: None,
                                linked: Vec::new(),
                                vault_path: Some(format!(".vault/memory/{id}.md")),
                                priority: None,
                                memory_kind: Some(memory_kind),
                                memory_status: Some(memory_status),
                                about_count: Some(about),
                                attested: Some(attested.to_string()),
                                verifies: Some(verifies.to_string()),
                                ..Default::default()
                            }
                        })
                        .collect();
                    Ok(ListVaultEntriesResponse {
                        entries,
                        next_cursor: None,
                        snapshot: None,
                    })
                }
                Some(Kind::Goal) => {
                    // A query: read-only open with the read-concurrency wait.
                    let repo = handle.repository_readonly()?;
                    let goals = repo
                        .vault_goal_list(status_filter.as_deref())
                        .map_err(repository_error)?;
                    let entries = goals
                        .into_iter()
                        .map(|goal| {
                            let status = match goal.status.as_str() {
                                "active" => VaultEntityStatus::Active,
                                "suspended" => VaultEntityStatus::Suspended,
                                "completed" => VaultEntityStatus::Completed,
                                _ => VaultEntityStatus::Unspecified,
                            };
                            VaultEntry {
                                kind: Kind::Goal as i32,
                                id: goal.name.clone(),
                                title: goal.developer.clone(),
                                status: Some(status as i32),
                                body: None,
                                created_at: None,
                                updated_at: None,
                                linked: goal.intent.clone().into_iter().collect(),
                                vault_path: Some(format!(".vault/goals/{}/_goal.md", goal.name)),
                                priority: None,
                                status_label: Some(goal.status.clone()),
                                started_at: Some(goal.started_at.clone()),
                                turns: Some(goal.turns),
                                ..Default::default()
                            }
                        })
                        .collect();
                    Ok::<_, Status>(ListVaultEntriesResponse {
                        entries,
                        next_cursor: None,
                        snapshot: None,
                    })
                }
                None => {
                    // A query: read-only open with the read-concurrency wait.
                    let repo = handle.repository_readonly()?;
                    // `vault list`: EVERY vault entry with its type, content
                    // size, and updated-at date, filtered by the path prefix
                    // and the parsed entry type — the same enumeration the
                    // local body performs.
                    let type_filter = entry_type
                        .as_deref()
                        .and_then(|raw| raw.parse::<VaultEntryType>().ok());
                    let metas = repo
                        .vault_list(&path_prefix, type_filter)
                        .map_err(repository_error)?;
                    let entries = metas
                        .into_iter()
                        .map(|meta| VaultEntry {
                            kind: Kind::Unspecified as i32,
                            id: meta.path.clone(),
                            title: String::new(),
                            status: None,
                            body: None,
                            created_at: None,
                            updated_at: None,
                            linked: Vec::new(),
                            vault_path: Some(meta.path.clone()),
                            priority: None,
                            content_size: Some(meta.content_size as u64),
                            updated_at_label: Some(meta.updated_at.clone()),
                            entry_type: Some(meta.entry_type.to_string()),
                            ..Default::default()
                        })
                        .collect();
                    Ok(ListVaultEntriesResponse {
                        entries,
                        next_cursor: None,
                        snapshot: None,
                    })
                }
                other => Err(domain_status(
                    ErrorCode::InvalidArgument,
                    format!("vault entity kind {other:?} lands with its slice (unimplemented)"),
                )),
            }
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn validate_vault_entity(
        &self,
        request: Request<ValidateVaultEntityRequest>,
    ) -> Result<Response<ValidateVaultEntityResponse>, Status> {
        let request = request.into_inner();
        let kind = Kind::try_from(request.kind)
            .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown vault entity kind"))?;
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ValidateVaultEntity", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let subject = request.subject.ok_or_else(|| {
            domain_status(ErrorCode::InvalidArgument, "validate subject is required")
        })?;
        let response = tokio::task::spawn_blocking(move || {
            // A query: read-only open with the read-concurrency wait.
            let repo = handle.repository_readonly()?;
            let id = match subject {
                validate_vault_entity_request::Subject::Id(id) => id,
                validate_vault_entity_request::Subject::Document(_) => {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "document-bytes validation lands with its slice",
                    ))
                }
            };
            let issues = match kind {
                Kind::Intent => {
                    let entry = repo.vault_intent_show(&id).map_err(repository_error)?;
                    let frontmatter = parse_frontmatter(&entry)?;
                    // Consult the stored attestation exactly like the CLI
                    // bridge: a fresh attested node validates as itself; a
                    // raw (unattested or stale) node validates as-is.
                    let raw_node =
                        lift_intent(&frontmatter, &body_of(&entry)).map_err(|error| {
                            domain_status(
                                ErrorCode::InvalidArgument,
                                format!("could not lift intent: {error}"),
                            )
                        })?;
                    let node = fresh_attested_intent(&repo, &id, &frontmatter, &body_of(&entry))
                        .unwrap_or(raw_node);
                    let report = validate_intent(&node);
                    report
                        .results
                        .iter()
                        .map(|violation| {
                            format!(
                                "[{}] {}{}: {}",
                                violation.shape,
                                violation.focus_node,
                                violation
                                    .path
                                    .as_ref()
                                    .map(|p| format!(" ({p})"))
                                    .unwrap_or_default(),
                                violation.message
                            )
                        })
                        .collect::<Vec<_>>()
                }
                Kind::Memory => {
                    let vault_path = format!("memory/{id}.md");
                    let entry = repo
                        .vault_retrieve(&vault_path)
                        .map_err(repository_error)?
                        .ok_or_else(|| domain_status(ErrorCode::NotFound, "memory not found"))?;
                    let frontmatter = parse_frontmatter(&entry)?;
                    let raw_node =
                        lift_memory(&frontmatter, &body_of(&entry)).map_err(|error| {
                            domain_status(
                                ErrorCode::InvalidArgument,
                                format!("could not lift memory: {error}"),
                            )
                        })?;
                    let node = fresh_attested_memory(&repo, &id, &frontmatter, &body_of(&entry))
                        .unwrap_or(raw_node);
                    let report = validate_memory(&node);
                    report
                        .results
                        .iter()
                        .map(|violation| {
                            format!(
                                "[{}] {}{}: {}",
                                violation.shape,
                                violation.focus_node,
                                violation
                                    .path
                                    .as_ref()
                                    .map(|p| format!(" ({p})"))
                                    .unwrap_or_default(),
                                violation.message
                            )
                        })
                        .collect::<Vec<_>>()
                }
                other => {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        format!("vault entity kind {other:?} lands with its slice (unimplemented)"),
                    ))
                }
            };
            let valid = issues.is_empty();
            Ok::<_, Status>(ValidateVaultEntityResponse { valid, issues })
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn sync_vault(
        &self,
        request: Request<SyncVaultRequest>,
    ) -> Result<Response<SyncVaultResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("SyncVault", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let handle_for_task = handle.clone();
        let response = tokio::task::spawn_blocking({
            let handle = handle_for_task;
            move || {
                let repo = handle.repository()?;
                let synced = repo.vault_record_working_copy().map_err(repository_error)?;
                Ok::<_, Status>(SyncVaultResponse {
                    entities_synced: synced.len() as u32,
                    meta: response_meta(&meta),
                })
            }
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn init_vault(
        &self,
        request: Request<InitVaultRequest>,
    ) -> Result<Response<InitVaultResponse>, Status> {
        // `atomic vault init` — inside an EXISTING repository (it requires
        // one), so it routes normally: has_vault → init_vault → track the
        // .vault tree → record the defaults (the local body's exact flow).
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("InitVault", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let (already, vault_dir, recorded) = tokio::task::spawn_blocking(move || {
            let (repo, workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            let working_copy = workspace.working_copy();
            if repo.has_vault().unwrap_or(false) {
                return Ok::<_, Status>((true, None, false));
            }
            repo.init_vault().map_err(repository_error)?;
            let vault_dir = repo.vault_dir().display().to_string();
            // Track every vault file (the local body's recursive add).
            if repo.vault_dir().exists() {
                add_vault_files_recursive(&repo, working_copy, &repo.vault_dir())?;
            }
            // Record the defaults as their own change; NothingToRecord is
            // the local body's silent no-op, other failures only log.
            let header = atomic_core::change::ChangeHeader::new("Initialize vault");
            let options = atomic_repository::RecordOptions::new()
                .add_path(".vault")
                .detect_raw_renames(false);
            let recorded = match repo.record(working_copy, header, options) {
                Ok(_) => true,
                Err(atomic_repository::RecordError::NothingToRecord) => false,
                Err(_) => false,
            };
            Ok((false, Some(vault_dir), recorded))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(InitVaultResponse {
            already_initialized: already,
            vault_dir,
            recorded,
            meta: response_meta(&meta),
        }))
    }

    async fn get_vault_context(
        &self,
        request: Request<GetVaultContextRequest>,
    ) -> Result<Response<GetVaultContextResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("GetVaultContext", Some(&handle));
        let max = request.max_entries.unwrap_or(20).max(1) as usize;
        let kinds = request.kinds;
        // The retrieval seeds and render knobs from `atomic vault context`
        // (add-only): the gather+rank runs in the handler; the CLI renders.
        let query = request.query;
        let intent = request.intent;
        let files = request.files;
        let budget_chars = request.budget_chars.unwrap_or(8000) as usize;
        let include_body = request.include_body.unwrap_or(false);
        let handle_for_task = handle.clone();
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let (entries, context) = tokio::task::spawn_blocking(move || {
            // A query: read-only open with the read-concurrency wait.
            let repo = handle_for_task.repository_readonly()?;
            let mut entries = Vec::new();
            let want = |kind: Kind| kinds.is_empty() || kinds.contains(&(kind as i32));
            if want(Kind::Intent) {
                for info in repo.vault_intent_list(None).map_err(repository_error)? {
                    entries.push(VaultEntry {
                        kind: Kind::Intent as i32,
                        id: info.id.clone(),
                        title: info.title.clone(),
                        status: intent_status_to_proto(&info.status).map(|s| s as i32),
                        body: None,
                        created_at: None,
                        updated_at: None,
                        linked: Vec::new(),
                        vault_path: None,
                        priority: Some(info.priority.clone()),
                        ..Default::default()
                    });
                }
            }
            if want(Kind::Memory) {
                for meta in repo
                    .vault_list("memory/", Some(VaultEntryType::Memory))
                    .map_err(repository_error)?
                {
                    let id = meta
                        .path
                        .trim_end_matches(".md")
                        .trim_start_matches("memory/")
                        .to_string();
                    entries.push(VaultEntry {
                        kind: Kind::Memory as i32,
                        id: id.clone(),
                        title: id,
                        status: None,
                        body: None,
                        created_at: None,
                        updated_at: None,
                        linked: Vec::new(),
                        vault_path: Some(format!(".vault/{}", meta.path)),
                        priority: None,
                        ..Default::default()
                    });
                }
            }
            entries.truncate(max);
            // The ranked, budgeted memory context — the ported
            // gather+rank domain computation behind `atomic vault context`.
            let context = context_ranking::context_items(
                &repo,
                &query,
                intent.as_deref(),
                &files,
                max,
                budget_chars,
                include_body,
            )?;
            Ok::<_, Status>((entries, context))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(GetVaultContextResponse {
            entries,
            context,
            snapshot: None,
        }))
    }

    async fn export_vault(
        &self,
        request: Request<ExportVaultRequest>,
    ) -> Result<Response<ExportVaultResponse>, Status> {
        // `atomic vault materialize` — inflate vault entries to markdown
        // in the working copy. The domain calls (vault_materialize /
        // vault_materialize_all) run handler-side; the CLI prints the
        // local report from the wire-carried counts.
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ExportVault", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let path = request.path.clone().filter(|path| !path.is_empty());
        let (count, materialized_path) = tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            match path {
                Some(path) => {
                    repo.vault_materialize(&path).map_err(repository_error)?;
                    Ok::<_, Status>((1, Some(path)))
                }
                None => {
                    let count = repo.vault_materialize_all().map_err(repository_error)?;
                    Ok((count as u32, None))
                }
            }
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(ExportVaultResponse {
            exported: count,
            materialized_path,
            meta: response_meta(&meta),
        }))
    }

    async fn delete_vault_entity(
        &self,
        request: Request<DeleteVaultEntityRequest>,
    ) -> Result<Response<DeleteVaultEntityResponse>, Status> {
        let request = request.into_inner();
        let kind = Kind::try_from(request.kind)
            .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown vault entity kind"))?;
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("DeleteVaultEntity", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let id = request.id.clone();
        let result = tokio::task::spawn_blocking(move || match kind {
            // `atomic intent delete` — the unstarted-backlog guard is the
            // domain's (vault_intent_delete); the normalized id + removed
            // file ride the response so the client prints the local report.
            Kind::Intent => {
                let repo = handle.repository()?;
                let result = repo.vault_intent_delete(&id).map_err(repository_error)?;
                Ok::<_, Status>((result.id, result.intent_file))
            }
            other => Err(domain_status(
                ErrorCode::InvalidArgument,
                format!("vault entity kind {other:?} lands with its slice"),
            )),
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(DeleteVaultEntityResponse {
            id: result.0,
            vault_path: format!(".vault/{}", result.1),
            meta: response_meta(&meta),
        }))
    }

    async fn link_vault_entities(
        &self,
        request: Request<LinkVaultEntitiesRequest>,
    ) -> Result<Response<LinkVaultEntitiesResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("LinkVaultEntities", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let source = request
            .source
            .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "source entity required"))?;
        let target = request
            .target
            .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "target entity required"))?;
        let source_kind = Kind::try_from(source.kind)
            .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown source kind"))?;
        let target_kind = Kind::try_from(target.kind)
            .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown target kind"))?;
        if !matches!((source_kind, target_kind), (Kind::Intent, Kind::Goal)) {
            return Err(domain_status(
                ErrorCode::InvalidArgument,
                "linking is implemented for intent → goal",
            ));
        }
        let intent_id = source.id.clone();
        let goal_name = target.id.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            repo.vault_intent_link(&intent_id, &goal_name)
                .map_err(repository_error)?;
            Ok::<_, Status>(())
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(LinkVaultEntitiesResponse {
            meta: response_meta(&meta),
        }))
    }
}

// ---------------------------------------------------------------------------
// AttestationService
// ---------------------------------------------------------------------------

/// One resolved attestation's detail (schema "atomic.attestation.detail.v1"):
/// the domain payload plus the per-view coverage rows (in the view
/// listing's order, zero-total views skipped), so the client renders the
/// detail with the same code the local body runs.
fn attestation_detail_bundle(
    repo: &Repository,
    hash: &atomic_core::types::Hash,
    attest: &Attestation,
) -> Result<VersionedBytes, Status> {
    let mut coverage: Vec<Value> = Vec::new();
    if let Ok(views) = repo.list_views() {
        for view_name in &views {
            let Ok(history) =
                repo.log(atomic_repository::history::HistoryOptions::default().view(view_name))
            else {
                continue;
            };
            let total = history.len();
            if total == 0 {
                continue;
            }
            let covered = history
                .iter()
                .filter(|entry| attest.covers_change(&entry.hash))
                .count();
            coverage.push(serde_json::json!({
                "view": view_name,
                "covered": covered,
                "total": total,
            }));
        }
    }
    let payload = serde_json::json!({
        "hash": hash.to_base32(),
        "attestation": attest,
        "coverage": coverage,
    });
    serde_json::to_vec(&payload)
        .map(|payload| VersionedBytes {
            schema: "atomic.attestation.detail.v1".to_string(),
            payload,
        })
        .map_err(|error| Status::internal(error.to_string()))
}

pub struct AttestationImpl {
    pub state: Arc<DaemonState>,
}

struct IdentityMaterial {
    identity: Identity,
    keypair: atomic_identity::keypair::KeyPair,
}

fn load_identity(name: Option<&str>) -> Result<IdentityMaterial, Status> {
    let store = IdentityStore::open_default()
        .map_err(|error| domain_status(ErrorCode::Internal, format!("identity store: {error}")))?;
    let identity = match name {
        Some(name) => store.load_by_name(name).map_err(|error| {
            domain_status(ErrorCode::NotFound, format!("identity '{name}': {error}"))
        })?,
        None => store
            .get_default()
            .map_err(|error| {
                domain_status(ErrorCode::Internal, format!("default identity: {error}"))
            })?
            .ok_or_else(|| domain_status(ErrorCode::NotFound, "no default identity selected"))?,
    };
    let keypair = store
        .load_keypair(&identity.id, None)
        .map_err(|error| domain_status(ErrorCode::Internal, format!("signing key: {error}")))?;
    Ok(IdentityMaterial { identity, keypair })
}

/// Load an identity for VERIFICATION only — the public key, never a secret
/// key. The external-signing flow (`--prepare`/`--signed`) must work for an
/// identity whose key is held elsewhere, so it may not touch the key store.
/// The name is required: an unbound caller cannot borrow the daemon's
/// default human key by signing as nobody (CONTRACT "Sandbox and vault
/// writes": unbound grants cannot use daemon default human keys).
fn load_identity_public(name: &str) -> Result<Identity, Status> {
    let store = IdentityStore::open_default()
        .map_err(|error| domain_status(ErrorCode::Internal, format!("identity store: {error}")))?;
    if name.is_empty() {
        return Err(domain_status(
            ErrorCode::InvalidArgument,
            "an externally-signed attestation names the identity it is signed by",
        ));
    }
    store
        .load_by_name(name)
        .map_err(|error| domain_status(ErrorCode::NotFound, format!("identity '{name}': {error}")))
}

/// The pre-attest gate the CLI's `attest` body applies: refuse violations
/// signing cannot fill (everything except `proof` and `attributedTo`), so a
/// signer never signs a node recording would refuse anyway.
fn refuse_unfillable(node: &CanonicalNode) -> Result<(), Status> {
    let report = validate_intent(node);
    let blocking: Vec<_> = report
        .results
        .iter()
        .filter(|v| !matches!(v.path.as_deref(), Some("proof") | Some("attributedTo")))
        .collect();
    if !blocking.is_empty() {
        return Err(domain_status(
            ErrorCode::PreconditionFailed,
            format!("the intent does not conform: {report}"),
        ));
    }
    Ok(())
}

/// blake3 source hash of (frontmatter + body) — the attestation freshness
/// anchor, mirroring the CLI bridges.
fn source_content_hash(frontmatter: &Map<String, Value>, body: &str) -> String {
    let fm = serde_json::to_string(frontmatter).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(fm.as_bytes());
    hasher.update(b"\0");
    hasher.update(body.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn write_all(path: &std::path::Path, bytes: &[u8]) -> Result<(), Status> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| domain_status(ErrorCode::Internal, format!("sidecar dir: {e}")))?;
    }
    std::fs::write(path, bytes)
        .map_err(|e| domain_status(ErrorCode::Internal, format!("sidecar write: {e}")))
}

#[tonic::async_trait]
impl attestation_service_server::AttestationService for AttestationImpl {
    async fn prepare_attestation(
        &self,
        request: Request<PrepareAttestationRequest>,
    ) -> Result<Response<PrepareAttestationResponse>, Status> {
        let request = request.into_inner();
        let target = request.target.clone().ok_or_else(|| {
            domain_status(ErrorCode::InvalidArgument, "attestation target required")
        })?;
        let target_kind = target.kind.ok_or_else(|| {
            domain_status(
                ErrorCode::InvalidArgument,
                "attestation target kind required",
            )
        })?;
        let handle =
            self.state
                .resolve(request.repository.as_ref().ok_or_else(|| {
                    domain_status(ErrorCode::InvalidArgument, "repository required")
                })?)?;
        self.state.log_rpc("PrepareAttestation", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let identity_name = request.identity_did.clone();

        let response = tokio::task::spawn_blocking(move || match target_kind {
            TargetKind::IntentId(intent_id) => {
                let repo = handle.repository()?;
                let entry = repo
                    .vault_intent_show(&intent_id)
                    .map_err(repository_error)?;
                let frontmatter = parse_frontmatter(&entry)?;
                let body = body_of(&entry);
                // No key here: everything that must agree with verification —
                // the author, the content hash, the canonical bytes — is
                // computed now, so the external signer only ever signs bytes.
                let identity = load_identity_public(&identity_name)?;
                let node = lift_intent(&frontmatter, &body).map_err(|error| {
                    domain_status(ErrorCode::InvalidArgument, format!("attest: {error}"))
                })?;
                refuse_unfillable(&node)?;
                let prepared = proof::prepare_attestation(node.to_value(), &identity.public_key);
                let document = serde_json::to_string(&prepared.value)
                    .map_err(|e| Status::internal(e.to_string()))?;
                Ok::<_, Status>(PrepareAttestationResponse {
                    document,
                    signing_bytes: prepared.signing_bytes,
                    snapshot: None,
                })
            }
            other => Err(domain_status(
                ErrorCode::InvalidArgument,
                format!("attestation target {other:?} lands with its slice"),
            )),
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn record_attestation(
        &self,
        request: Request<RecordAttestationRequest>,
    ) -> Result<Response<RecordAttestationResponse>, Status> {
        let request = request.into_inner();
        let target = request.target.ok_or_else(|| {
            domain_status(ErrorCode::InvalidArgument, "attestation target required")
        })?;
        let target_kind = target.kind.ok_or_else(|| {
            domain_status(
                ErrorCode::InvalidArgument,
                "attestation target kind required",
            )
        })?;
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("RecordAttestation", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let identity_name = request.identity_did.clone();
        let meta = request.meta.clone();
        let caller_signature = request.signature.clone();

        let response = tokio::task::spawn_blocking(move || match target_kind {
            TargetKind::IntentId(intent_id) => {
                let repo = handle.repository()?;
                let entry = repo
                    .vault_intent_show(&intent_id)
                    .map_err(repository_error)?;
                let frontmatter = parse_frontmatter(&entry)?;
                let body = body_of(&entry);
                let (node, public_key): (CanonicalNode, _) = if caller_signature.is_empty() {
                    // Sign here, with this machine's key — the plain `attest`.
                    let material = load_identity(if identity_name.is_empty() {
                        None
                    } else {
                        Some(&identity_name)
                    })?;
                    (
                        lift_and_attest(&frontmatter, &body, &material.identity, &material.keypair)
                            .map_err(|error| {
                                domain_status(
                                    ErrorCode::InvalidArgument,
                                    format!("attest: {error}"),
                                )
                            })?,
                        material.identity.public_key.clone(),
                    )
                } else {
                    // The key was held elsewhere: verify the caller's
                    // signature against the target AS IT IS NOW (a stale or
                    // altered intent hashes differently, so an old signature
                    // cannot land), and attach exactly the proof it earned.
                    // No secret key is loaded.
                    let identity = load_identity_public(&identity_name)?;
                    let unattested = lift_intent(&frontmatter, &body).map_err(|error| {
                        domain_status(ErrorCode::InvalidArgument, format!("attest: {error}"))
                    })?;
                    refuse_unfillable(&unattested)?;
                    let prepared =
                        proof::prepare_attestation(unattested.to_value(), &identity.public_key);
                    let signature = Signature::from_slice(&caller_signature).map_err(|error| {
                        domain_status(ErrorCode::InvalidArgument, format!("signature: {error}"))
                    })?;
                    signature
                        .verify(&prepared.signing_bytes, &identity.public_key)
                        .map_err(|_| {
                            domain_status(
                                ErrorCode::PreconditionFailed,
                                format!(
                                    "the signature does not verify against intent {intent_id} \
                                     as it is now (re-run --prepare)",
                                ),
                            )
                        })?;
                    (
                        serde_json::from_value(proof::attach_proof(
                            prepared.value,
                            &identity.public_key,
                            &signature,
                        ))
                        .map_err(|e| {
                            domain_status(ErrorCode::Internal, format!("attested intent: {e}"))
                        })?,
                        identity.public_key.clone(),
                    )
                };
                let report = validate_intent(&node);
                if !report.conforms {
                    return Err(domain_status(
                        ErrorCode::PreconditionFailed,
                        format!("attested intent does not conform: {}", report),
                    ));
                }
                verify(&node, &public_key).map_err(|error| {
                    domain_status(ErrorCode::Internal, format!("self-check failed: {error}"))
                })?;
                let normalized = normalized_id(&repo, &intent_id);
                let sanitized = sanitize_id(&normalized);
                let source_hash = source_content_hash(&frontmatter, &body);
                let node_json =
                    serde_json::to_value(&node).map_err(|e| Status::internal(e.to_string()))?;
                let sidecar = repo
                    .dot_dir()
                    .join("canonical")
                    .join("intents")
                    .join(&sanitized)
                    .join("attested.jsonld");
                let sidecar_body = serde_json::json!({
                    "node": node_json,
                    "source": { "sourceContentHash": source_hash },
                });
                write_all(
                    &sidecar,
                    serde_json::to_vec_pretty(&sidecar_body)
                        .map_err(|e| Status::internal(e.to_string()))?
                        .as_slice(),
                )?;
                let vault_path = format!("attestations/{sanitized}/attested.md");
                let attested_body = format!(
                    "{}\n",
                    serde_json::to_string_pretty(&node_json)
                        .map_err(|e| Status::internal(e.to_string()))?
                );
                let attested_frontmatter = serde_json::json!({
                    "intentId": intent_id,
                    "sourceContentHash": source_hash,
                });
                repo.vault_store(
                    &vault_path,
                    VaultEntryType::Attestation,
                    attested_body.into_bytes(),
                    attested_frontmatter.to_string(),
                )
                .map_err(repository_error)?;
                repo.vault_materialize(&vault_path)
                    .map_err(repository_error)?;
                Ok::<_, Status>(RecordAttestationResponse {
                    attestation: Some(AttestationInfo {
                        id: normalized,
                        target: Some(AttestationTarget {
                            kind: Some(TargetKind::IntentId(intent_id.clone())),
                        }),
                        identity_did: node.attributed_to.clone().unwrap_or_default(),
                        signature: None,
                        recorded_at: Some(prost_types::Timestamp {
                            seconds: chrono::Utc::now().timestamp(),
                            nanos: 0,
                        }),
                        vault_path: Some(format!(".vault/{vault_path}")),
                        sidecar_path: Some(sidecar.display().to_string()),
                    }),
                    meta: response_meta(&meta),
                })
            }
            TargetKind::MemoryId(memory_id) => {
                let repo = handle.repository()?;
                let vault_path = format!("memory/{memory_id}.md");
                let entry = repo
                    .vault_retrieve(&vault_path)
                    .map_err(repository_error)?
                    .ok_or_else(|| domain_status(ErrorCode::NotFound, "memory not found"))?;
                let frontmatter = parse_frontmatter(&entry)?;
                let body = body_of(&entry);
                let material = load_identity(if identity_name.is_empty() {
                    None
                } else {
                    Some(&identity_name)
                })?;
                let node: MemoryNode = lift_and_attest_memory(
                    &frontmatter,
                    &body,
                    &material.identity,
                    &material.keypair,
                )
                .map_err(|error| {
                    domain_status(ErrorCode::InvalidArgument, format!("attest: {error}"))
                })?;
                let report = validate_memory(&node);
                if !report.conforms {
                    return Err(domain_status(
                        ErrorCode::PreconditionFailed,
                        format!("attested memory does not conform: {}", report),
                    ));
                }
                verify_memory(&node, &material.keypair.public).map_err(|error| {
                    domain_status(ErrorCode::Internal, format!("self-check failed: {error}"))
                })?;
                let sanitized = sanitize_id(&memory_id);
                let source_hash = source_content_hash(&frontmatter, &body);
                let node_json =
                    serde_json::to_value(&node).map_err(|e| Status::internal(e.to_string()))?;
                let sidecar = repo
                    .dot_dir()
                    .join("canonical")
                    .join("memory")
                    .join(&sanitized)
                    .join("attested.jsonld");
                let sidecar_body = serde_json::json!({
                    "node": node_json,
                    "source": { "sourceContentHash": source_hash },
                });
                write_all(
                    &sidecar,
                    serde_json::to_vec_pretty(&sidecar_body)
                        .map_err(|e| Status::internal(e.to_string()))?
                        .as_slice(),
                )?;
                let attested_vault_path = format!("attestations/memory/{sanitized}/attested.md");
                let attested_body = format!(
                    "{}\n",
                    serde_json::to_string_pretty(&node_json)
                        .map_err(|e| Status::internal(e.to_string()))?
                );
                let attested_frontmatter = serde_json::json!({
                    "memoryId": memory_id,
                    "sourceContentHash": source_hash,
                });
                repo.vault_store(
                    &attested_vault_path,
                    VaultEntryType::Attestation,
                    attested_body.into_bytes(),
                    attested_frontmatter.to_string(),
                )
                .map_err(repository_error)?;
                repo.vault_materialize(&attested_vault_path)
                    .map_err(repository_error)?;
                Ok(RecordAttestationResponse {
                    attestation: Some(AttestationInfo {
                        id: memory_id.clone(),
                        target: Some(AttestationTarget {
                            kind: Some(TargetKind::MemoryId(memory_id.clone())),
                        }),
                        identity_did: node.attributed_to.clone().unwrap_or_default(),
                        signature: None,
                        recorded_at: Some(prost_types::Timestamp {
                            seconds: chrono::Utc::now().timestamp(),
                            nanos: 0,
                        }),
                        vault_path: Some(format!(".vault/{attested_vault_path}")),
                        sidecar_path: Some(sidecar.display().to_string()),
                    }),
                    meta: response_meta(&meta),
                })
            }
            other => Err(domain_status(
                ErrorCode::InvalidArgument,
                format!("attestation target {other:?} lands with its slice"),
            )),
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn verify_attestation(
        &self,
        request: Request<VerifyAttestationRequest>,
    ) -> Result<Response<VerifyAttestationResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("VerifyAttestation", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let target = match request
            .target
            .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "verify target required"))?
        {
            verify_attestation_request::Target::AttestationId(_id) => {
                return Ok(Response::new(VerifyAttestationResponse {
                    valid: false,
                    reason: Some("attestation-id verification lands with its slice".to_string()),
                }))
            }
            verify_attestation_request::Target::Subject(subject) => {
                subject.kind.ok_or_else(|| {
                    domain_status(ErrorCode::InvalidArgument, "verify target kind required")
                })?
            }
        };
        let identity_name = request.identity_name.clone();
        let response = tokio::task::spawn_blocking(move || match target {
            TargetKind::IntentId(intent_id) => {
                let repo = handle.repository()?;
                let entry = repo
                    .vault_intent_show(&intent_id)
                    .map_err(repository_error)?;
                let frontmatter = parse_frontmatter(&entry)?;
                let body = body_of(&entry);
                let normalized = normalized_id(&repo, &intent_id);
                let vault_path = format!("attestations/{}/attested.md", sanitize_id(&normalized));
                let attested = repo
                    .vault_retrieve(&vault_path)
                    .map_err(repository_error)?
                    .ok_or_else(|| {
                        domain_status(
                            ErrorCode::NotFound,
                            "intent has no attestation; run `atomic intent attest`",
                        )
                    })?;
                let node: CanonicalNode = serde_json::from_str(body_of(&attested).trim_end())
                    .map_err(|e| {
                        domain_status(
                            ErrorCode::Internal,
                            format!("attested node unreadable: {e}"),
                        )
                    })?;
                let recorded_hash = serde_json::from_str::<Value>(&attested.frontmatter_json)
                    .ok()
                    .and_then(|v| {
                        v.get("sourceContentHash")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                if recorded_hash.as_deref() != Some(&source_content_hash(&frontmatter, &body)) {
                    return Ok(VerifyAttestationResponse {
                        valid: false,
                        reason: Some(
                            "attestation is stale: the intent changed since it was signed"
                                .to_string(),
                        ),
                    });
                }
                let material = load_identity(identity_name.as_deref())?;
                match verify(&node, &material.keypair.public) {
                    Ok(()) => Ok(VerifyAttestationResponse {
                        valid: true,
                        reason: node.attributed_to.clone(),
                    }),
                    Err(error) => Ok(VerifyAttestationResponse {
                        valid: false,
                        reason: Some(error.to_string()),
                    }),
                }
            }
            TargetKind::MemoryId(memory_id) => {
                let repo = handle.repository()?;
                let vault_path = format!("memory/{memory_id}.md");
                let entry = repo
                    .vault_retrieve(&vault_path)
                    .map_err(repository_error)?
                    .ok_or_else(|| domain_status(ErrorCode::NotFound, "memory not found"))?;
                let frontmatter = parse_frontmatter(&entry)?;
                let body = body_of(&entry);
                let attested_path = format!(
                    "attestations/memory/{}/attested.md",
                    sanitize_id(&memory_id)
                );
                let attested = repo
                    .vault_retrieve(&attested_path)
                    .map_err(repository_error)?
                    .ok_or_else(|| {
                        domain_status(
                            ErrorCode::NotFound,
                            "memory has no attestation; run `atomic memory attest`",
                        )
                    })?;
                let node: MemoryNode = serde_json::from_str(body_of(&attested).trim_end())
                    .map_err(|e| {
                        domain_status(
                            ErrorCode::Internal,
                            format!("attested node unreadable: {e}"),
                        )
                    })?;
                let recorded_hash = serde_json::from_str::<Value>(&attested.frontmatter_json)
                    .ok()
                    .and_then(|v| {
                        v.get("sourceContentHash")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                if recorded_hash.as_deref() != Some(&source_content_hash(&frontmatter, &body)) {
                    return Ok(VerifyAttestationResponse {
                        valid: false,
                        reason: Some(
                            "attestation is stale: the memory changed since it was signed"
                                .to_string(),
                        ),
                    });
                }
                let material = load_identity(identity_name.as_deref())?;
                match verify_memory(&node, &material.keypair.public) {
                    Ok(()) => Ok(VerifyAttestationResponse {
                        valid: true,
                        reason: node.attributed_to.clone(),
                    }),
                    Err(error) => Ok(VerifyAttestationResponse {
                        valid: false,
                        reason: Some(error.to_string()),
                    }),
                }
            }
            other => Err(domain_status(
                ErrorCode::InvalidArgument,
                format!("attestation target {other:?} lands with its slice"),
            )),
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn list_attestations(
        &self,
        request: Request<ListAttestationsRequest>,
    ) -> Result<Response<ListAttestationsResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ListAttestations", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;

        // `agent attest --hash <prefix>`: resolve ONE attestation by hash
        // or prefix (the hash names an attestation; the change-oriented Log
        // cannot resolve it) and serve its full detail — the domain
        // payload plus the per-view coverage — as versioned-opaque bytes
        // the client renders with its existing detail code.
        if let Some(prefix) = request.hash_prefix.clone().filter(|p| !p.is_empty()) {
            let handle_for_task = handle.clone();
            let detail = tokio::task::spawn_blocking(move || -> Result<VersionedBytes, Status> {
                let repo = handle_for_task.repository()?;
                // Exact match first, then the prefix scan — the same
                // resolution the CLI's find_by_prefix performs.
                let mut matches: Vec<(atomic_core::types::Hash, Attestation)> = Vec::new();
                if let Some(hash) =
                    atomic_core::types::Hash::from_base32(prefix.as_bytes())
                {
                    if let Ok(attest) = repo.load_attestation(&hash) {
                        return attestation_detail_bundle(&repo, &hash, &attest);
                    }
                }
                let prefix_upper = prefix.to_uppercase();
                for result in repo.change_store().iter_attestations() {
                    let hash = match result {
                        Ok(hash) => hash,
                        Err(_) => continue,
                    };
                    if hash.to_base32().starts_with(&prefix_upper) {
                        if let Ok(attest) = repo.load_attestation(&hash) {
                            matches.push((hash, attest));
                        }
                    }
                }
                match matches.len() {
                    0 => Err(domain_status(
                        ErrorCode::NotFound,
                        format!("No attestation found matching '{prefix}'"),
                    )),
                    1 => Ok(attestation_detail_bundle(
                        &repo,
                        &matches[0].0,
                        &matches[0].1,
                    )?),
                    n => Err(domain_status(
                        ErrorCode::NotFound,
                        format!(
                            "Ambiguous hash prefix '{prefix}' matches {n} attestations. Be more specific."
                        ),
                    )),
                }
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))??;
            return Ok(Response::new(ListAttestationsResponse {
                attestations: Vec::new(),
                next_cursor: None,
                detail_bundle: Some(detail),
                summary_bundle: None,
            }));
        }

        // `agent attest --summary [--view V] [--pending P]`: the AI
        // provenance summary (AI/Human/Needs-attention/System
        // classification over the view's changes), served as
        // versioned-opaque bytes the client renders with its existing
        // summary code. `--pending` classifies only the view's delta
        // relative to the parent.
        if let Some(summary_view) = request.summary_view.clone().filter(|v| !v.is_empty()) {
            let pending_parent = request.pending_parent.clone();
            let handle_for_task = handle.clone();
            let summary = tokio::task::spawn_blocking(move || -> Result<VersionedBytes, Status> {
                let repo = handle_for_task.repository()?;
                let summary = if let Some(parent) = pending_parent.clone().filter(|p| !p.is_empty())
                {
                    repo.provenance_summary_pending(&summary_view, &parent)
                        .map_err(repository_error)?
                } else {
                    repo.provenance_summary(&summary_view)
                        .map_err(repository_error)?
                };
                // The draft-view hint source: the summarized view's
                // recorded parent, exactly what the local body reads.
                let parent = repo
                    .get_view_info(&summary_view)
                    .ok()
                    .and_then(|info| info.parent_name);
                let payload = serde_json::json!({
                    "summary": summary,
                    "parent": parent,
                });
                serde_json::to_vec(&payload)
                    .map(|payload| VersionedBytes {
                        schema: "atomic.provenance.summary.v1".to_string(),
                        payload,
                    })
                    .map_err(|error| Status::internal(error.to_string()))
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))??;
            return Ok(Response::new(ListAttestationsResponse {
                attestations: Vec::new(),
                next_cursor: None,
                detail_bundle: None,
                summary_bundle: Some(summary),
            }));
        }

        // The agent-attest read path filters by view; the unfiltered listing
        // walks the whole audit graph and lands with its slice.
        let view = request
            .filter
            .as_ref()
            .and_then(|filter| filter.kind.clone());
        let view = match view {
            Some(TargetKind::GraphStats(stats)) => stats.session_id,
            _ => None,
        };
        let view_name = view.unwrap_or_else(|| handle.current_view());
        let handle_for_task = handle.clone();
        let attestations = tokio::task::spawn_blocking(move || {
            let repo = handle_for_task.repository()?;
            let results = repo
                .find_attestations_for_view(&view_name)
                .map_err(repository_error)?;
            Ok::<_, Status>(
                results
                    .into_iter()
                    .map(|(hash, _, _covered)| AttestationInfo {
                        id: format!(
                            "attestation:{}",
                            super::state::hash_bytes(&hash)
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        ),
                        target: Some(AttestationTarget {
                            kind: Some(TargetKind::GraphStats(GraphStatsRef {
                                session_id: None,
                                turn_number: None,
                                change_hash: Some(super::convert::hash_proto(&hash)),
                            })),
                        }),
                        identity_did: String::new(),
                        signature: None,
                        recorded_at: None,
                        vault_path: None,
                        sidecar_path: None,
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(ListAttestationsResponse {
            attestations,
            next_cursor: None,
            detail_bundle: None,
            summary_bundle: None,
        }))
    }
}

// ---------------------------------------------------------------------------
// KnowledgeService
// ---------------------------------------------------------------------------

pub struct KnowledgeImpl {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl knowledge_service_server::KnowledgeService for KnowledgeImpl {
    async fn query_graph(
        &self,
        request: Request<QueryGraphRequest>,
    ) -> Result<Response<QueryGraphResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("QueryGraph", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let query = request
            .query
            .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "query kind required"))?;
        let limit = if request.limit == 0 {
            10
        } else {
            request.limit as usize
        };
        let root = handle.root.clone();
        let response = tokio::task::spawn_blocking(move || match query {
            QueryOneof::KgSearch(search) => {
                let repo = handle.repository()?;
                // The candidate pool the CLI's --pool names (default 5000)
                // — the pool is what lets lower-scored kinds survive the
                // diversity selection, so it rides the request.
                let pool = search.pool.map(|pool| pool as usize).unwrap_or(5000);
                let nodes = if let Some(kind_filter) =
                    search.kind.clone().filter(|kind| !kind.is_empty())
                {
                    // The --kind form: search the full pool, keep the
                    // kind's nodes, take the limit AFTER filtering — the
                    // local post-filter order.
                    let kind = kind_filter.to_lowercase();
                    repo.vault_kg_search(&search.query, pool, Some(pool))
                        .map_err(repository_error)?
                        .into_iter()
                        .filter(|node| node.kind.to_lowercase() == kind)
                        .take(limit)
                        .map(|node| crate::daemon::services_query::kg_node_proto(node.clone()))
                        .collect::<Vec<_>>()
                } else {
                    repo.vault_kg_search(&search.query, limit, Some(pool))
                        .map_err(repository_error)?
                        .into_iter()
                        .map(|node| crate::daemon::services_query::kg_node_proto(node.clone()))
                        .collect()
                };
                Ok::<_, Status>(QueryGraphResponse {
                    nodes,
                    edges: Vec::new(),
                    hits: Vec::new(),
                    answer: None,
                    plan_result: None,
                    snapshot: None,
                    graph_collected: None,
                    calls_present: false,
                    ask_result: None,
                    ask_elapsed_ms: None,
                })
            }
            QueryOneof::Neighbors(neighbors) => {
                let repo = handle.repository()?;
                let (nodes, edges) = crate::daemon::services_query::neighbors_impl(
                    &repo,
                    &neighbors.node_id,
                    neighbors.depth.unwrap_or(1) as u8,
                )?;
                Ok(QueryGraphResponse {
                    nodes,
                    edges,
                    hits: Vec::new(),
                    answer: None,
                    plan_result: None,
                    snapshot: None,
                    graph_collected: None,
                    calls_present: false,
                    ask_result: None,
                    ask_elapsed_ms: None,
                })
            }
            QueryOneof::Entity(entity) => {
                // `query entities <file>` — tree-sitter extraction over the
                // working-copy file. This is a working-copy READ, not a
                // repository database operation.
                let source =
                    std::fs::read_to_string(root.join(&entity.node_id)).map_err(|error| {
                        domain_status(
                            ErrorCode::NotFound,
                            format!("cannot read {}: {error}", entity.node_id),
                        )
                    })?;
                let mut registry = atomic_semantic::ParserRegistry::new();
                let entities = registry.extract(&source, &entity.node_id);
                let nodes = entities
                    .into_iter()
                    .map(|entity| KgNode {
                        id: format!("entity:{}:{}:{}", entity.file, entity.name, entity.line),
                        node_type: entity.kind.to_string(),
                        name: Some(entity.name.clone()),
                        data: entity.signature.clone().map(String::into_bytes),
                        score: None,
                        ..Default::default()
                    })
                    .collect();
                Ok(QueryGraphResponse {
                    nodes,
                    edges: Vec::new(),
                    hits: Vec::new(),
                    answer: None,
                    plan_result: None,
                    snapshot: None,
                    graph_collected: None,
                    calls_present: false,
                    ask_result: None,
                    ask_elapsed_ms: None,
                })
            }
            QueryOneof::Callers(callers) => {
                let repo = handle.repository_readonly()?;
                // The local body's domain: the 1-hop neighbors subgraph,
                // filtered to CALLS edges pointing at the entity.
                let (nodes, edges) =
                    crate::daemon::services_query::neighbors_impl(&repo, &callers.node_id, 1)?;
                let subgraph = repo
                    .vault_kg_neighbors(&callers.node_id, 1)
                    .map_err(repository_error)?;
                let calls_present = subgraph
                    .edges
                    .iter()
                    .any(|edge| edge.kind.to_uppercase() == "CALLS");
                // The caller edges + the caller nodes (their summaries ride
                // the nodes so the CLI renders the caller lines).
                let caller_edges: Vec<crate::atomic::KgEdge> = subgraph
                    .edges
                    .iter()
                    .filter(|edge| {
                        edge.kind.to_uppercase() == "CALLS" && edge.to_id == callers.node_id
                    })
                    .map(|edge| crate::daemon::services_query::kg_edge_proto(edge.clone()))
                    .collect();
                let caller_ids: std::collections::HashSet<&str> = subgraph
                    .edges
                    .iter()
                    .filter(|edge| {
                        edge.kind.to_uppercase() == "CALLS" && edge.to_id == callers.node_id
                    })
                    .map(|edge| edge.from_id.as_str())
                    .collect();
                let caller_nodes: Vec<crate::atomic::KgNode> = subgraph
                    .nodes
                    .iter()
                    .filter(|node| caller_ids.contains(node.id.as_str()))
                    .map(|node| crate::daemon::services_query::kg_node_proto(node.clone()))
                    .collect();
                let _ = (nodes, edges);
                Ok(QueryGraphResponse {
                    nodes: caller_nodes,
                    edges: caller_edges,
                    hits: Vec::new(),
                    answer: None,
                    plan_result: None,
                    snapshot: None,
                    graph_collected: None,
                    calls_present,
                    ask_result: None,
                    ask_elapsed_ms: None,
                })
            }
            QueryOneof::GraphSlice(graph) => {
                // `atomic query graph` — the full seed/expand/filter/cap
                // build (the local command's algorithm, ported verbatim).
                let repo = handle.repository_readonly()?;
                let query = graph.query.clone().unwrap_or_default();
                if query.is_empty() {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "graph seed query required",
                    ));
                }
                let limit = if graph.seed_limit.unwrap_or(0) == 0 {
                    10
                } else {
                    graph.seed_limit.unwrap() as usize
                };
                let depth = graph.depth.unwrap_or(1).min(2) as u8;
                let kinds = graph.kinds.clone().unwrap_or_else(|| "all".to_string());
                let allow_all = kinds.trim().eq_ignore_ascii_case("all");
                let allowed_kinds: std::collections::HashSet<String> = if allow_all {
                    std::collections::HashSet::new()
                } else {
                    kinds
                        .split(',')
                        .map(|kind| kind.trim().to_lowercase())
                        .filter(|kind| !kind.is_empty())
                        .collect()
                };
                let kind_allowed = |kind: &str| -> bool {
                    allow_all || allowed_kinds.contains(&kind.to_lowercase())
                };
                let changes_per_seed = graph.changes_per_seed.unwrap_or(5) as usize;
                let cap_changes = changes_per_seed > 0;
                // 1. Seed: search, filtered to allowed kinds, take limit.
                let seed_nodes: Vec<atomic_core::pristine::vault::KgNode> = repo
                    .vault_kg_search(&query, limit * 3, None)
                    .map_err(repository_error)?
                    .into_iter()
                    .filter(|node| kind_allowed(&node.kind))
                    .take(limit)
                    .collect();
                if seed_nodes.is_empty() {
                    return Ok(QueryGraphResponse {
                        nodes: Vec::new(),
                        edges: Vec::new(),
                        hits: Vec::new(),
                        answer: None,
                        plan_result: None,
                        snapshot: None,
                        graph_collected: Some(0),
                        calls_present: false,
                        ask_result: None,
                        ask_elapsed_ms: None,
                    });
                }
                // 2. Expand: per-seed neighbor subgraphs, kind-filtered,
                //    change-capped per seed.
                let mut node_map: std::collections::HashMap<
                    String,
                    atomic_core::pristine::vault::KgNode,
                > = std::collections::HashMap::new();
                let mut edge_set: std::collections::HashSet<(String, String, String)> =
                    std::collections::HashSet::new();
                let mut all_edges: Vec<atomic_core::pristine::vault::KgEdge> = Vec::new();
                for seed in &seed_nodes {
                    node_map
                        .entry(seed.id.clone())
                        .or_insert_with(|| seed.clone());
                    let subgraph = repo
                        .vault_kg_neighbors(&seed.id, depth)
                        .map_err(repository_error)?;
                    let mut changes_for_seed: usize = 0;
                    for node in subgraph.nodes {
                        if !kind_allowed(&node.kind) {
                            continue;
                        }
                        if node.kind == "change" && cap_changes {
                            if node_map.contains_key(&node.id) {
                                continue;
                            }
                            if changes_for_seed >= changes_per_seed {
                                continue;
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
                        if from_kind_ok
                            && to_kind_ok
                            && (node_map.contains_key(&edge.from_id)
                                || node_map.contains_key(&edge.to_id))
                        {
                            let key = (edge.from_id.clone(), edge.to_id.clone(), edge.kind.clone());
                            if edge_set.insert(key) {
                                all_edges.push(edge);
                            }
                        }
                    }
                }
                // 3. Isolated-node removal (seeds always kept).
                let connected_ids: std::collections::HashSet<String> = all_edges
                    .iter()
                    .flat_map(|edge| [edge.from_id.clone(), edge.to_id.clone()])
                    .collect();
                let seed_ids: std::collections::HashSet<&str> =
                    seed_nodes.iter().map(|node| node.id.as_str()).collect();
                let mut nodes: Vec<atomic_core::pristine::vault::KgNode> = node_map
                    .into_values()
                    .filter(|node| {
                        connected_ids.contains(&node.id) || seed_ids.contains(node.id.as_str())
                    })
                    .collect();
                nodes.sort_by(|left, right| left.id.cmp(&right.id));
                let final_ids: std::collections::HashSet<&str> =
                    nodes.iter().map(|node| node.id.as_str()).collect();
                all_edges.retain(|edge| {
                    final_ids.contains(edge.from_id.as_str())
                        && final_ids.contains(edge.to_id.as_str())
                });
                // 4. Cap at max_nodes (0 = the format default: 200 DOT /
                //    5000 HTML — the caller's own default applies).
                let max_nodes = graph.max_nodes.unwrap_or(0) as usize;
                let mut collected = nodes.len() as u32;
                if max_nodes > 0 && nodes.len() > max_nodes {
                    let kept_ids: std::collections::HashSet<&str> = nodes[..max_nodes]
                        .iter()
                        .map(|node| node.id.as_str())
                        .collect();
                    all_edges.retain(|edge| {
                        kept_ids.contains(edge.from_id.as_str())
                            && kept_ids.contains(edge.to_id.as_str())
                    });
                    nodes.truncate(max_nodes);
                }
                let _ = &collected;
                collected = nodes.len() as u32;
                let _ = &collected;
                Ok(QueryGraphResponse {
                    nodes: nodes
                        .iter()
                        .map(|node| crate::daemon::services_query::kg_node_proto(node.clone()))
                        .collect(),
                    edges: all_edges
                        .iter()
                        .map(|edge| crate::daemon::services_query::kg_edge_proto(edge.clone()))
                        .collect(),
                    hits: Vec::new(),
                    answer: None,
                    plan_result: None,
                    snapshot: None,
                    graph_collected: None,
                    calls_present: false,
                    ask_result: None,
                    ask_elapsed_ms: None,
                })
            }
            QueryOneof::Plan(plan) => {
                // `atomic query plan` — the structured plan executes against
                // the repository; the domain PlanResult JSON rides the
                // response for both the --json render (byte parity) and the
                // human summary.
                let repo = handle.repository()?;
                let plan_json = String::from_utf8_lossy(&plan.plan_json).into_owned();
                let parsed = atomic_repository::parse_plan(&plan_json).map_err(repository_error)?;
                let result =
                    atomic_repository::execute_plan(&repo, &parsed).map_err(repository_error)?;
                let payload = serde_json::to_vec(&result)
                    .map_err(|error| Status::internal(format!("plan encode: {error}")))?;
                Ok(QueryGraphResponse {
                    nodes: Vec::new(),
                    edges: Vec::new(),
                    hits: Vec::new(),
                    answer: None,
                    plan_result: Some(payload),
                    snapshot: None,
                    graph_collected: None,
                    calls_present: false,
                    ask_result: None,
                    ask_elapsed_ms: None,
                })
            }
            QueryOneof::RagAsk(ask) => {
                // `atomic query ask` — the agentic tool loop runs over the
                // repository (the LLM provider resolves from the same
                // environment/config); the full AgentResult rides the wire.
                let repo = handle.repository_readonly()?;
                let llm = atomic_repository::resolve_llm_provider().ok_or_else(|| {
                    domain_status(
                        ErrorCode::InvalidArgument,
                        "No API key configured. Set ANTHROPIC_API_KEY or OPENAI_API_KEY.",
                    )
                })?;
                let executor = atomic_repository::RepoToolExecutor::new(&repo);
                let max_turns = ask.max_turns.unwrap_or(5);
                let verbose = ask.verbose.unwrap_or(false);
                let config = atomic_repository::AgentConfig {
                    system_prompt: crate::daemon::services_query::ASK_SYSTEM_PROMPT.to_string(),
                    max_turns: max_turns as u8,
                    max_tokens: 4096,
                    verbose,
                };
                let start = std::time::Instant::now();
                let result =
                    atomic_repository::run_tool_loop_sync(&llm, &executor, &ask.question, &config)
                        .map_err(|error| {
                            domain_status(ErrorCode::Repository, format!("LLM error: {error}"))
                        })?;
                let elapsed = start.elapsed();
                let payload = serde_json::to_vec(&result)
                    .map_err(|error| Status::internal(format!("ask encode: {error}")))?;
                Ok(QueryGraphResponse {
                    nodes: Vec::new(),
                    edges: Vec::new(),
                    hits: Vec::new(),
                    answer: Some(result.answer),
                    plan_result: None,
                    snapshot: None,
                    graph_collected: None,
                    calls_present: false,
                    ask_result: Some(VersionedBytes {
                        schema: "atomic.query.ask.v1".to_string(),
                        payload,
                    }),
                    ask_elapsed_ms: Some(elapsed.as_millis() as u64),
                })
            }
            other => Err(domain_status(
                ErrorCode::InvalidArgument,
                format!("query kind {other:?} lands with its slice"),
            )),
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }

    async fn maintain_knowledge_graph(
        &self,
        request: Request<MaintainKnowledgeGraphRequest>,
    ) -> Result<Response<MaintainKnowledgeGraphResponse>, Status> {
        let request = request.into_inner();
        let action = KnowledgeMaintainAction::try_from(request.action)
            .unwrap_or(KnowledgeMaintainAction::Unspecified);
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("MaintainKnowledgeGraph", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let response = tokio::task::spawn_blocking(move || {
            let (repo, workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            let working_copy = workspace.working_copy();
            let processed = match action {
                KnowledgeMaintainAction::Enrich => {
                    // Per-change scope (`enrich --changes`): enrich only
                    // the named changes' nodes.
                    if !request.changes.is_empty() {
                        let mut total: u64 = 0;
                        for hash in &request.changes {
                            if hash.value.len() != 32 {
                                return Err(domain_status(
                                    ErrorCode::InvalidArgument,
                                    "hash must be 32 bytes",
                                ));
                            }
                            let mut bytes = [0u8; 32];
                            bytes.copy_from_slice(&hash.value);
                            repo.kg_enrich_change(working_copy, &atomic_core::types::Merkle(bytes))
                                .map_err(repository_error)?;
                            total += 1;
                        }
                        return Ok::<_, Status>(MaintainKnowledgeGraphResponse {
                            processed: total,
                            provider: None,
                            dimensions: None,
                            path: None,
                            meta: response_meta(&meta),
                        });
                    }
                    // The CLI --rebuild flag rides the scope field ("rebuild").
                    if request.scope.as_deref() == Some("rebuild") {
                        repo.kg_clear_derived().map_err(repository_error)?;
                    }
                    let mut total: u64 = 0;
                    total += repo.kg_enrich_views().map_err(repository_error)? as u64;
                    total += repo
                        .kg_enrich_files(working_copy)
                        .map_err(repository_error)? as u64;
                    total += repo
                        .kg_enrich_modules(working_copy)
                        .map_err(repository_error)? as u64;
                    total += repo
                        .kg_enrich_changes(working_copy)
                        .map_err(repository_error)? as u64;
                    total += repo
                        .kg_enrich_entities(working_copy)
                        .map_err(repository_error)? as u64;
                    total += repo
                        .kg_enrich_includes(working_copy)
                        .map_err(repository_error)? as u64;
                    total += repo
                        .kg_enrich_calls(working_copy)
                        .map_err(repository_error)? as u64;
                    total
                }
                KnowledgeMaintainAction::Reindex => {
                    repo.vault_reindex_kg().map_err(repository_error)? as u64
                }
                // `atomic query embed` — the provider resolves from the
                // same environment/config the CLI reads; the sync embed
                // function + the hash fallback are the domain's.
                KnowledgeMaintainAction::Embed => {
                    // The embed target rides the scope field (the per-path
                    // form; absent = embed everything).
                    let scope = request.scope.clone();
                    let provider = atomic_repository::resolve_embedding_provider();
                    let dims = provider.dimensions;
                    let config = atomic_repository::EmbedConfig {
                        max_chunk_tokens: 512,
                        dimensions: dims,
                    };
                    let embed_fn = |text: &str| -> Vec<f32> {
                        provider
                            .embed_sync(&[text.to_string()])
                            .ok()
                            .and_then(|v| v.into_iter().next())
                            .unwrap_or_else(|| atomic_repository::hash_embed(text, dims))
                    };
                    match scope {
                        Some(path) if !path.is_empty() => {
                            let count = repo
                                .vault_embed(&path, &embed_fn, &config)
                                .map_err(repository_error)?;
                            return Ok::<_, Status>(MaintainKnowledgeGraphResponse {
                                processed: count as u64,
                                provider: Some(provider.model.clone()),
                                dimensions: Some(dims as u32),
                                path: Some(path),
                                meta: response_meta(&meta),
                            });
                        }
                        _ => {
                            let count = repo
                                .vault_embed_all(&embed_fn, &config)
                                .map_err(repository_error)?;
                            return Ok(MaintainKnowledgeGraphResponse {
                                processed: count as u64,
                                provider: Some(provider.model.clone()),
                                dimensions: Some(dims as u32),
                                path: None,
                                meta: response_meta(&meta),
                            });
                        }
                    }
                }
                other => {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        format!("maintain action {other:?} lands with its slice"),
                    ))
                }
            };
            Ok::<_, Status>(MaintainKnowledgeGraphResponse {
                processed,
                provider: None,
                dimensions: None,
                path: None,
                meta: response_meta(&meta),
            })
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(response))
    }
}

// ---------------------------------------------------------------------------
// ProvenanceService — the transitional turn-dispatch seam
// ---------------------------------------------------------------------------

pub struct ProvenanceImpl {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl provenance_service_server::ProvenanceService for ProvenanceImpl {
    async fn dispatch_turn_event(
        &self,
        request: Request<DispatchTurnEventRequest>,
    ) -> Result<Response<DispatchTurnEventResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("DispatchTurnEvent", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let agent_id = request.agent_id.clone();
        let display = request
            .agent_display_name
            .clone()
            .unwrap_or_else(|| agent_id.clone());
        let identity = request.agent_identity.clone();
        let body = request
            .event
            .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "typed event required"))?;
        let hook_type = hook_type(&body.event_type).ok_or_else(|| {
            domain_status(
                ErrorCode::InvalidArgument,
                format!("unknown event type '{}'", body.event_type),
            )
        })?;
        let raw_json = body
            .raw_json
            .as_ref()
            .filter(|bytes| !bytes.is_empty())
            .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
        let event = TurnEvent {
            session_id: body.session_id.clone(),
            event_type: hook_type,
            transcript_path: None,
            prompt: body.prompt.clone(),
            tool_name: body.tool_name.clone(),
            tool_use_id: body.tool_use_id.clone(),
            timestamp: body
                .timestamp
                .as_ref()
                .map(timestamp_domain)
                .unwrap_or_else(chrono::Utc::now),
            raw_json,
        };

        // The daemon hosts the turn orchestrator: it owns the repository
        // databases, so the dispatch (session view, recording, journal
        // checkpoint) runs in-process with a direct journal sink.
        let sink = DirectJournalSink::open(&handle.root)
            .map_err(|error| domain_status(ErrorCode::ProvenanceStore, error))?;
        let mut orchestrator =
            TurnOrchestrator::new(handle.root.clone())
                .await
                .map_err(|error| {
                    domain_status(ErrorCode::Internal, format!("orchestrator: {error}"))
                })?;
        orchestrator.set_agent(agent_id, display);
        orchestrator.set_agent_identity(identity);
        orchestrator.set_journal_sink(Arc::new(sink));
        let result = orchestrator.dispatch(event).await.map_err(|error| {
            domain_status(ErrorCode::Internal, format!("dispatch failed: {error}"))
        })?;

        let recorded = result.change_recorded.as_ref();
        Ok(Response::new(DispatchTurnEventResponse {
            session_id: result.session_id.clone(),
            recorded: recorded.is_some(),
            change_hash: recorded.map(|outcome| super::convert::hash_proto(&outcome.hash)),
            view: result.view.clone(),
            files: recorded
                .map(|outcome| outcome.recorded_file_list().to_vec())
                .unwrap_or_default(),
            warnings: result.warnings.clone(),
            meta: response_meta(&meta),
        }))
    }

    async fn reserve_turn(
        &self,
        request: Request<ReserveTurnRequest>,
    ) -> Result<Response<ReserveTurnResponse>, Status> {
        crate::daemon::services_provenance::reserve_turn_impl(&self.state, request).await
    }

    async fn append_envelopes(
        &self,
        request: Request<AppendEnvelopesRequest>,
    ) -> Result<Response<AppendEnvelopesResponse>, Status> {
        crate::daemon::services_provenance::append_envelopes_impl(&self.state, request).await
    }

    async fn prepare_checkpoint(
        &self,
        request: Request<PrepareCheckpointRequest>,
    ) -> Result<Response<PrepareCheckpointResponse>, Status> {
        crate::daemon::services_provenance::prepare_checkpoint_impl(&self.state, request).await
    }

    async fn load_frozen_envelopes(
        &self,
        request: Request<LoadFrozenEnvelopesRequest>,
    ) -> Result<Response<LoadFrozenEnvelopesResponse>, Status> {
        crate::daemon::services_provenance::load_frozen_envelopes_impl(&self.state, request).await
    }

    async fn bind_checkpoint_hash(
        &self,
        request: Request<BindCheckpointHashRequest>,
    ) -> Result<Response<BindCheckpointHashResponse>, Status> {
        crate::daemon::services_provenance::bind_checkpoint_hash_impl(&self.state, request).await
    }

    async fn acknowledge_checkpoint(
        &self,
        request: Request<AcknowledgeCheckpointRequest>,
    ) -> Result<Response<AcknowledgeCheckpointResponse>, Status> {
        crate::daemon::services_provenance::acknowledge_checkpoint_impl(&self.state, request).await
    }

    async fn update_turn(
        &self,
        request: Request<UpdateTurnRequest>,
    ) -> Result<Response<UpdateTurnResponse>, Status> {
        crate::daemon::services_provenance::update_turn_impl(&self.state, request).await
    }

    async fn get_turn(
        &self,
        request: Request<GetTurnRequest>,
    ) -> Result<Response<GetTurnResponse>, Status> {
        crate::daemon::services_provenance::get_turn_impl(&self.state, request).await
    }

    async fn get_session(
        &self,
        request: Request<GetSessionRequest>,
    ) -> Result<Response<GetSessionResponse>, Status> {
        crate::daemon::services_query::get_session_impl(&self.state, request).await
    }

    async fn list_sessions(
        &self,
        request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ListSessions", Some(&handle));
        let limit = if request.limit == 0 {
            10
        } else {
            request.limit as usize
        };
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let handle_for_task = handle.clone();
        let sessions = tokio::task::spawn_blocking(move || {
            let repo = handle_for_task.repository()?;
            let ledgers = repo.list_session_ledgers(limit).map_err(repository_error)?;
            // Vault-derived session→intents map (the same map the client
            // built locally — one implementation, served here).
            let mut intent_map: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            if let Ok(manifest) = repo.vault_manifest() {
                for (key, summary) in &manifest.intents {
                    if let Some(session_id) = &summary.session {
                        let label = if summary.human_key.is_empty() {
                            key.clone()
                        } else {
                            summary.human_key.clone()
                        };
                        let ids = intent_map.entry(session_id.clone()).or_default();
                        if !ids.contains(&label) {
                            ids.push(label);
                        }
                    }
                }
            }
            // The complete ledgers as versioned-opaque bytes (JSON of the
            // {record, turns} pairs — atomic.session.ledgers.v1); the
            // client renders listings/JSON from the domain shape.
            let ledgers_bundle = serde_json::to_vec(&ledgers)
                .ok()
                .map(|payload| VersionedBytes {
                    schema: "atomic.session.ledgers.v1".to_string(),
                    payload,
                });
            let mut intent_counts = Vec::with_capacity(ledgers.len());
            let summaries = ledgers
                .into_iter()
                .map(|(record, turns)| {
                    // Prefer explicit plan ids from the turns; the vault
                    // map covers the rest.
                    let from_turns: std::collections::HashSet<&str> =
                        turns.iter().filter_map(|t| t.plan_id.as_deref()).collect();
                    let intent_count = if !from_turns.is_empty() {
                        from_turns.len()
                    } else {
                        intent_map
                            .get(&record.session_id)
                            .map(|ids| ids.len())
                            .unwrap_or(0)
                    };
                    intent_counts.push(intent_count as u32);
                    SessionSummary {
                        session_id: record.session_id,
                        view: record.view_name,
                        turn_count: record.turn_count.max(turns.len() as u32),
                        started_at: Some(prost_types::Timestamp {
                            seconds: record.started_at,
                            nanos: 0,
                        }),
                        ended_at: record.ended_at.map(|ended| prost_types::Timestamp {
                            seconds: ended,
                            nanos: 0,
                        }),
                    }
                })
                .collect::<Vec<_>>();
            Ok::<_, Status>((summaries, ledgers_bundle, intent_counts))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        let (sessions, ledgers_bundle, intent_counts) = sessions;
        Ok(Response::new(ListSessionsResponse {
            sessions,
            ledgers_bundle,
            intent_counts,
        }))
    }

    async fn fork_session(
        &self,
        request: Request<ForkSessionRequest>,
    ) -> Result<Response<ForkSessionResponse>, Status> {
        // `atomic session fork` — the domain fork (parent manifest, child
        // session, the inherited turns) runs handler-side; the wire carries
        // both manifest hashes for the local fork report.
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ForkSession", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let session_id = request.session_id.clone();
        let child = request.forked_session_id.clone();
        let child_for_report = child.clone();
        let at_turn = request.at_turn.unwrap_or(0);
        let (parent_hash, child_hash) = tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            repo.fork_session(&session_id, at_turn, &child)
                .map_err(repository_error)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(ForkSessionResponse {
            session_id: child_for_report,
            parent_manifest: Some(super::convert::hash_proto(&parent_hash)),
            child_manifest: Some(super::convert::hash_proto(&child_hash)),
            fork_turn: Some(at_turn),
            meta: response_meta(&meta),
        }))
    }

    async fn rebuild_session_index(
        &self,
        request: Request<RebuildSessionIndexRequest>,
    ) -> Result<Response<RebuildSessionIndexResponse>, Status> {
        // `atomic session rebuild` — the domain rebuild runs handler-side;
        // the counts ride the response for the local report.
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("RebuildSessionIndex", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let (indexed, skipped, corrupt) = tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            repo.rebuild_session_index().map_err(repository_error)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(RebuildSessionIndexResponse {
            indexed: indexed as u64,
            already_present: skipped as u64,
            corrupt: corrupt as u64,
            meta: response_meta(&meta),
        }))
    }

    async fn get_provenance(
        &self,
        _request: Request<GetProvenanceRequest>,
    ) -> Result<Response<GetProvenanceResponse>, Status> {
        Err(Status::unimplemented("GetProvenance lands with its slice"))
    }

    async fn export_provenance(
        &self,
        request: Request<ExportProvenanceRequest>,
    ) -> Result<Response<ExportProvenanceResponse>, Status> {
        export_provenance_impl(self.state.clone(), request).await
    }

    async fn explain_turns(
        &self,
        request: Request<ExplainTurnsRequest>,
    ) -> Result<Response<ExplainTurnsResponse>, Status> {
        explain_turns_impl(self.state.clone(), request).await
    }
}

// ---------------------------------------------------------------------------
// ExplainTurns — the `agent explain` domain (the transcript extraction,
// the Claude CLI generation, the graph anchoring, and the save-backs)
// ---------------------------------------------------------------------------

/// The local `get_condensed_text`: the change's unhashed section first,
/// then the session transcript file condensed on the fly.
#[allow(clippy::too_many_arguments)]
fn explain_condensed_text(
    change: &atomic_core::change::Change,
    transcript_path: Option<&std::path::Path>,
    agent_name: &str,
    files_touched: &[String],
) -> String {
    use atomic_agent::transcript;
    if let Some(unhashed) = transcript::extract_unhashed(change) {
        if !unhashed.condensed_text.is_empty() {
            return unhashed.condensed_text;
        }
        if !unhashed.condensed_transcript.is_empty() {
            let files: Vec<String> = unhashed
                .tools_used
                .iter()
                .flat_map(|tool| tool.files_affected.clone())
                .collect();
            return transcript::format_condensed(&unhashed.condensed_transcript, &files);
        }
    }
    if let Some(path) = transcript_path {
        if let Ok(raw) = std::fs::read(path) {
            let format = if agent_name.contains("gemini") {
                "json"
            } else {
                "jsonl"
            };
            let entries = transcript::condense_transcript(&raw, format);
            if entries.is_empty() {
                return String::new();
            }
            let files: Vec<String> = if !change.file_ops().is_empty() {
                change
                    .file_ops()
                    .iter()
                    .map(|file_op| file_op.path().to_string())
                    .collect()
            } else {
                files_touched.to_vec()
            };
            return transcript::format_condensed(&entries, &files);
        }
    }
    String::new()
}

/// The local explain body's domain, in ONE blocking pass: the turn list
/// on the session's view, then per turn the transcript extraction, the
/// Claude CLI generation, the graph anchoring, and the save-backs.
async fn explain_turns_impl(
    state: std::sync::Arc<DaemonState>,
    request: Request<ExplainTurnsRequest>,
) -> Result<Response<ExplainTurnsResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("ExplainTurns", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();
    let session_id = request.session_id.clone();
    let view_name = request.view_name.clone();
    let transcript_path = request
        .transcript_path
        .clone()
        .map(std::path::PathBuf::from);
    let files_touched = request.files_touched.clone();
    let agent_name = request.agent_name.clone();
    let root = handle.root.clone();
    let model = request.model.clone();
    let turn = request.turn;
    let all = request.all;
    let save = request.save;

    let (session_has_no_turns, results) = tokio::task::spawn_blocking(move || {
        let repo = handle.repository()?;
        let entries = repo
            .log(atomic_repository::HistoryOptions::with_headers().view(&view_name))
            .map_err(|error| match error {
                atomic_repository::RepositoryError::ViewNotFound { name } => {
                    domain_status(ErrorCode::View, format!("view '{name}' does not exist"))
                }
                other => repository_error(other),
            })?;
        if entries.is_empty() {
            return Ok::<_, Status>((true, Vec::new()));
        }
        let len = entries.len();
        let turns: Vec<usize> = if all {
            (0..len).collect()
        } else if let Some(turn_number) = turn {
            let index = (turn_number as usize).checked_sub(1).ok_or_else(|| {
                domain_status(ErrorCode::InvalidArgument, "Turn number must be >= 1")
            })?;
            if index >= len {
                return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    format!(
                        "Turn {} not found. Session has {} turn{}.",
                        turn_number,
                        len,
                        if len == 1 { "" } else { "s" }
                    ),
                ));
            }
            vec![index]
        } else {
            vec![len - 1]
        };

        let mut results = Vec::with_capacity(turns.len());
        for index in turns {
            let entry = entries
                .get(index)
                .ok_or_else(|| domain_status(ErrorCode::Changes, "turn index out of range"))?;
            let change = repo.load_change(&entry.hash).map_err(|error| {
                domain_status(
                    ErrorCode::Changes,
                    format!("Failed to load change {}: {error}", entry.hash.to_base32()),
                )
            })?;
            let message = change.hashed.header.message.clone();
            let turn_number = (index + 1) as u32;
            let condensed = explain_condensed_text(
                &change,
                transcript_path.as_deref(),
                &agent_name,
                &files_touched,
            );
            if condensed.is_empty() {
                results.push(ExplainTurnResult {
                    turn_number,
                    message,
                    change: Some(super::convert::hash_proto(&entry.hash)),
                    no_transcript: true,
                    reasoning: None,
                    generation_error: None,
                    anchored: 0,
                    saved: false,
                    save_error: None,
                    context_saved: None,
                    context_save_error: None,
                });
                continue;
            }

            // The files for this turn — FileOps paths first, then the
            // unhashed tool summaries, then the session's touched files.
            let files: Vec<String> = if !change.file_ops().is_empty() {
                change
                    .file_ops()
                    .iter()
                    .map(|file_op| file_op.path().to_string())
                    .collect()
            } else if let Some(unhashed) = atomic_agent::transcript::extract_unhashed(&change) {
                unhashed
                    .tools_used
                    .iter()
                    .flat_map(|tool| tool.files_affected.clone())
                    .collect()
            } else {
                files_touched.clone()
            };

            let generator = atomic_agent::transcript::ClaudeCliGenerator::new().with_model(&model);
            use atomic_agent::transcript::ReasoningGenerator as _;
            let reasoning = match generator.generate(&condensed, &files) {
                Ok(reasoning) => reasoning,
                Err(error) => {
                    results.push(ExplainTurnResult {
                        turn_number,
                        message,
                        change: Some(super::convert::hash_proto(&entry.hash)),
                        no_transcript: false,
                        reasoning: None,
                        generation_error: Some(error.to_string()),
                        anchored: 0,
                        saved: false,
                        save_error: None,
                        context_saved: None,
                        context_save_error: None,
                    });
                    continue;
                }
            };
            if reasoning.is_empty() {
                results.push(ExplainTurnResult {
                    turn_number,
                    message,
                    change: Some(super::convert::hash_proto(&entry.hash)),
                    no_transcript: false,
                    reasoning: None,
                    generation_error: Some(String::new()),
                    anchored: 0,
                    saved: false,
                    save_error: None,
                    context_saved: None,
                    context_save_error: None,
                });
                continue;
            }

            // Anchor code learnings to the CRDT graph.
            let mut reasoning = reasoning;
            let mut anchored = 0u32;
            if reasoning.has_code_learnings() {
                let file_ops = change.file_ops();
                if !file_ops.is_empty() {
                    atomic_agent::transcript::anchor_to_graph(
                        &mut reasoning.learnings.code,
                        file_ops,
                    );
                    anchored = reasoning
                        .learnings
                        .code
                        .iter()
                        .filter(|learning| learning.is_anchored())
                        .count() as u32;
                }
            }

            let reasoning_payload = serde_json::to_vec(&reasoning)
                .map_err(|error| Status::internal(format!("reasoning encode: {error}")))?;

            // Save back into the change's unhashed section (pushable) and
            // the learnings to the agent context file.
            let (saved, save_error, context_saved, context_save_error) = if save {
                let mut updated = change.clone();
                let mut unhashed_data = atomic_agent::transcript::extract_unhashed(&updated)
                    .unwrap_or_else(|| {
                        atomic_agent::transcript::UnhashedTurnData::new(
                            "",
                            0,
                            "unknown",
                            Vec::new(),
                            &[],
                        )
                    });
                unhashed_data = unhashed_data.with_reasoning(reasoning.clone());
                let mut saved = false;
                let mut save_error = None;
                match atomic_agent::transcript::attach_unhashed(&mut updated, &unhashed_data) {
                    Ok(()) => match repo.save_change(&updated) {
                        Ok(_) => saved = true,
                        Err(error) => save_error = Some(error.to_string()),
                    },
                    Err(error) => {
                        save_error = Some(format!("Failed to attach reasoning: {error}"));
                    }
                }
                let (context_saved, context_save_error) = if !reasoning.learnings.is_empty() {
                    match atomic_agent::learnings::save_learnings_to_context_file(
                        &root,
                        &agent_name,
                        &reasoning.learnings,
                    ) {
                        Ok(result) => (Some(result.to_string()), None),
                        Err(error) => (None, Some(error.to_string())),
                    }
                } else {
                    (None, None)
                };
                (saved, save_error, context_saved, context_save_error)
            } else {
                (false, None, None, None)
            };

            results.push(ExplainTurnResult {
                turn_number,
                message,
                change: Some(super::convert::hash_proto(&entry.hash)),
                no_transcript: false,
                reasoning: Some(VersionedBytes {
                    schema: "atomic.agent.reasoning.v1".to_string(),
                    payload: reasoning_payload,
                }),
                generation_error: None,
                anchored,
                saved,
                save_error,
                context_saved,
                context_save_error,
            });
        }
        Ok::<_, Status>((false, results))
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    let _ = session_id;

    Ok(Response::new(ExplainTurnsResponse {
        turns: results,
        session_has_no_turns,
        meta: response_meta(&meta),
    }))
}

// ---------------------------------------------------------------------------
// ExportProvenance — the `provenance trace/show` domain (add-only fields:
// the resolved change hash + per-graph trace bundles)
// ---------------------------------------------------------------------------

/// Build a unique activity id for a turn's graph — the CLI's
/// `activity_id_for` semantics: `<session_id>#<first-explained-change>`
/// (or just the session id when none are explained), so turnParent joins.
fn provenance_activity_id(graph: &atomic_core::change::ProvenanceGraph) -> String {
    use atomic_core::types::Base32;
    match graph.changes_explained.first() {
        Some(hash) => format!("{}#{}", graph.session_id, hash.to_base32()),
        None => graph.session_id.clone(),
    }
}

/// Map a loaded graph into the plain projector input — the CLI's
/// `map_graph_to_input` semantics (the turn parent loads the previous
/// graph and uses ITS activity id; best-effort when it cannot load).
fn provenance_map_input(
    repo: &atomic_repository::Repository,
    graph: &atomic_core::change::ProvenanceGraph,
    change_hash: &atomic_core::types::Hash,
    person_did: &str,
) -> (
    atomic_canonical::prov::ProvActivityInput,
    Option<atomic_core::types::Hash>,
) {
    use atomic_canonical::prov::{change_urn, ProvActivityInput};
    use atomic_core::types::Base32;
    let previous = graph.previous;
    let turn_parent = previous.and_then(|prev_hash| {
        repo.load_provenance_graph(&prev_hash)
            .ok()
            .map(|prev| atomic_canonical::prov::activity_urn(&provenance_activity_id(&prev)))
    });
    let agent_vendor = (!graph.agent_vendor.is_empty()).then(|| graph.agent_vendor.clone());
    let input = ProvActivityInput {
        change_id_base32: change_hash.to_base32(),
        activity_id: provenance_activity_id(graph),
        started_at: None,
        ended_at: None,
        agent_slug: atomic_canonical::prov::normalize_agent_slug(&graph.agent_name),
        agent_display_name: graph.agent_display_name.clone(),
        agent_vendor,
        person_did: person_did.to_string(),
        generated: graph
            .changes_explained
            .iter()
            .map(|hash| change_urn(&hash.to_base32()))
            .collect(),
        used: Vec::new(),
        turn_parent,
    };
    (input, previous)
}

/// The ProvActivityInput as the versioned bundle JSON (schema
/// "atomic.prov.input.v1") — the CLI deserializes the same shape.
fn prov_input_bundle(input: &atomic_canonical::prov::ProvActivityInput) -> Vec<u8> {
    serde_json::json!({
        "change_id_base32": input.change_id_base32,
        "activity_id": input.activity_id,
        "started_at": input.started_at,
        "ended_at": input.ended_at,
        "agent_slug": input.agent_slug,
        "agent_display_name": input.agent_display_name,
        "agent_vendor": input.agent_vendor,
        "person_did": input.person_did,
        "generated": input.generated,
        "used": input.used,
        "turn_parent": input.turn_parent,
    })
    .to_string()
    .into_bytes()
}

/// The ExportProvenance domain: resolve the target (URN/prefix/hash with
/// the CLI resolver's exact semantics), load the explaining graphs (the
/// REV_DEPS lookup with the disk-scan fallback, newest first), map each
/// to the projection input, and pre-walk the prior-turn chain the human
/// trace renders. The CLI projects/signs client-side over the bundles.
async fn export_provenance_impl(
    state: std::sync::Arc<DaemonState>,
    request: Request<ExportProvenanceRequest>,
) -> Result<Response<ExportProvenanceResponse>, Status> {
    use atomic_core::types::Base32;
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("ExportProvenance", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let target = request.target.clone();
    let exact = request.change.as_ref().and_then(|hash| {
        hash.value
            .clone()
            .try_into()
            .ok()
            .map(atomic_core::types::Merkle)
    });
    let person_did = request.person_did.clone().unwrap_or_default();
    let (resolved, bundles) = tokio::task::spawn_blocking(move || {
        let repo = handle.repository_readonly()?;
        // Resolve the target: the raw CLI string (URN / prefix / full
        // base32), else the request's exact hash.
        let change_hash = match &target {
            Some(target) => resolve_provenance_target(&repo, target)?,
            None => exact.ok_or_else(|| {
                domain_status(ErrorCode::InvalidArgument, "change target required")
            })?,
        };
        // The graphs that explain the change, newest first (the CLI's
        // load_graphs: REV_DEPS first, disk-scan fallback, error when
        // nothing explains it).
        let mut graphs = repo
            .find_provenance_for_change(&change_hash)
            .map_err(repository_error)?;
        if graphs.is_empty() {
            graphs = repo
                .find_provenance_for_change_scan(&change_hash)
                .map_err(repository_error)?;
        }
        if graphs.is_empty() {
            return Err(domain_status(
                ErrorCode::InvalidArgument,
                format!(
                    "no provenance graph explains change {}",
                    change_hash.to_base32()
                ),
            ));
        }
        graphs.sort_by_key(|(_, graph)| std::cmp::Reverse(graph.timestamp));
        let mut bundles = Vec::with_capacity(graphs.len());
        for (graph_hash, graph) in &graphs {
            let (input, previous) = provenance_map_input(&repo, graph, &change_hash, &person_did);
            // The prior-turn walk the trace render performs (≤64 hops; an
            // unloadable graph ends the chain with the empty marker, the
            // cap sets the truncated flag — the local render's two
            // distinct chain-end lines).
            let mut prior_activities = Vec::new();
            let mut chain_truncated = false;
            let mut cursor = previous;
            let mut depth = 0usize;
            while let Some(prev_hash) = cursor {
                depth += 1;
                if depth > 64 {
                    chain_truncated = true;
                    break;
                }
                match repo.load_provenance_graph(&prev_hash) {
                    Ok(prev) => {
                        prior_activities.push(atomic_canonical::prov::activity_urn(
                            &provenance_activity_id(&prev),
                        ));
                        cursor = prev.previous;
                    }
                    Err(_) => {
                        prior_activities.push(String::new());
                        break;
                    }
                }
            }
            let graph_payload = serde_json::to_vec(&graph)
                .map_err(|error| Status::internal(format!("graph encode: {error}")))?;
            bundles.push(ProvenanceTraceBundle {
                graph_hash: Some(super::convert::hash_proto(graph_hash)),
                graph: Some(VersionedBytes {
                    schema: "atomic.prov.graph.v1".to_string(),
                    payload: graph_payload,
                }),
                input: Some(VersionedBytes {
                    schema: "atomic.prov.input.v1".to_string(),
                    payload: prov_input_bundle(&input),
                }),
                prior_activities,
                chain_truncated,
            });
        }
        Ok::<_, Status>((change_hash, bundles))
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(ExportProvenanceResponse {
        prov_jsonld: Vec::new(),
        resolved_change: Some(super::convert::hash_proto(&resolved)),
        graphs: bundles,
    }))
}

/// The raw CLI target (bare hash, hash prefix, or URN) → the change hash —
/// the provenance command's resolver semantics verbatim.
fn resolve_provenance_target(
    repo: &atomic_repository::Repository,
    target: &str,
) -> Result<atomic_core::types::Merkle, Status> {
    use atomic_core::types::Base32;
    const URN_PREFIX: &str = "urn:atomic:change:";
    if let Some(base32) = target.strip_prefix(URN_PREFIX) {
        return atomic_core::types::Hash::from_base32(base32.as_bytes()).ok_or_else(|| {
            domain_status(
                ErrorCode::InvalidArgument,
                format!("invalid change base32 in URN: {target}"),
            )
        });
    }
    let mut matches = Vec::new();
    for result in repo.iter_changes() {
        let hash =
            result.map_err(|error| domain_status(ErrorCode::Repository, error.to_string()))?;
        if hash.to_base32().starts_with(target) {
            matches.push(hash);
        }
    }
    match matches.len() {
        0 => atomic_core::types::Hash::from_base32(target.as_bytes()).ok_or_else(|| {
            domain_status(ErrorCode::NotFound, format!("change not found: {target}"))
        }),
        1 => Ok(matches[0]),
        _ => {
            let list: Vec<String> = matches.iter().map(|hash| hash.to_base32()).collect();
            Err(domain_status(
                ErrorCode::InvalidArgument,
                format!(
                    "ambiguous change prefix {} (matches: {})",
                    target,
                    list.join(", ")
                ),
            ))
        }
    }
}
