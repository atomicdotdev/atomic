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
use atomic_canonical::{
    lift_and_attest, lift_and_attest_memory, verify, verify_memory, CanonicalNode, MemoryNode,
};
use atomic_core::pristine::VaultEntryType;
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
    /// node identity onto `memory/<name>.md`.
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

/// Does the memory stored at `memory/<id>.md` carry a fresh attestation
/// signed by `did` that verifies under `public_key`? The
/// identity-filtered listing's domain check — the same
/// DID-match-then-verify rule the CLI's attestation-aware memory listing
/// applies: only a current, same-signer, cryptographically valid
/// attestation passes (a stale or other-signed attestation does not).
fn memory_attests_identity(
    repo: &Repository,
    vault_path: &str,
    public_key: &atomic_identity::keypair::PublicKey,
    did: &str,
) -> bool {
    let Ok(Some(entry)) = repo.vault_retrieve(vault_path) else {
        return false;
    };
    let Ok(frontmatter) = parse_frontmatter(&entry) else {
        return false;
    };
    let body = body_of(&entry);
    let Some(id) = vault_path
        .strip_prefix("memory/")
        .and_then(|stem| stem.strip_suffix(".md"))
    else {
        return false;
    };
    let Some(node) = fresh_attested_memory(repo, id, &frontmatter, &body) else {
        return false;
    };
    // DID pre-check: a different (or absent) signer is not a match.
    if node.attributed_to.as_deref() != Some(did) {
        return false;
    }
    // Same signer ⇒ the only failure that can follow is a real
    // hash/signature failure.
    verify_memory(&node, public_key).is_ok()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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
                    }),
                    meta: response_meta(&meta),
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
                    }),
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
                        }),
                        meta: response_meta(&meta),
                    })
                }
                UpdateKind::Status(_) => Err(domain_status(
                    ErrorCode::InvalidArgument,
                    "memory status transitions are new revisions, not updates",
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
        let response = tokio::task::spawn_blocking(move || match kind {
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
                Ok::<_, Status>(GetVaultEntryResponse {
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
                    }),
                    snapshot: None,
                })
            }
            Kind::Memory => {
                // A query: read-only open with the read-concurrency wait.
                let repo = handle.repository_readonly()?;
                let vault_path = format!("memory/{id}.md");
                let entry = repo
                    .vault_retrieve(&vault_path)
                    .map_err(repository_error)?
                    .ok_or_else(|| domain_status(ErrorCode::NotFound, "memory not found"))?;
                Ok(GetVaultEntryResponse {
                    entry: Some(VaultEntry {
                        kind: Kind::Memory as i32,
                        id: id.clone(),
                        title: id.clone(),
                        status: None,
                        body: Some(body_of(&entry)),
                        created_at: None,
                        updated_at: None,
                        linked: Vec::new(),
                        vault_path: Some(format!(".vault/{vault_path}")),
                        priority: None,
                    }),
                    snapshot: None,
                })
            }
            other => Err(domain_status(
                ErrorCode::InvalidArgument,
                format!("vault entity kind {other:?} lands with its slice (unimplemented)"),
            )),
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
        let kind = request
            .kind
            .and_then(|k| Kind::try_from(k).ok())
            .unwrap_or(Kind::Intent);
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ListVaultEntries", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let identity = request.identity;
        let response = tokio::task::spawn_blocking(move || match kind {
            Kind::Intent => {
                // A query: read-only open with the read-concurrency wait.
                let repo = handle.repository_readonly()?;
                let infos = repo.vault_intent_list(None).map_err(repository_error)?;
                let entries = infos
                    .into_iter()
                    .map(|info| VaultEntry {
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
                    })
                    .collect();
                Ok::<_, Status>(ListVaultEntriesResponse {
                    entries,
                    next_cursor: None,
                    snapshot: None,
                })
            }
            Kind::Memory => {
                // A query: read-only open with the read-concurrency wait.
                let repo = handle.repository_readonly()?;
                let metas = repo
                    .vault_list("memory/", Some(VaultEntryType::Memory))
                    .map_err(repository_error)?;
                // `--identity`: keep only the memories whose attestation
                // verifies under that identity — the domain check the CLI's
                // attestation-aware listing performs (resolve the identity,
                // load each memory's fresh attested node, DID-match the
                // signer, verify the signature). A named-but-missing
                // identity is a hard error, matching the verify verbs.
                let verifier = identity
                    .map(|name| {
                        let store = IdentityStore::open_default().map_err(|error| {
                            domain_status(
                                ErrorCode::NotFound,
                                format!("identity store unavailable: {error}"),
                            )
                        })?;
                        let identity = store.load_by_name(&name).map_err(|_| {
                            domain_status(
                                ErrorCode::NotFound,
                                format!("identity '{name}' not found"),
                            )
                        })?;
                        Ok::<_, Status>((
                            identity.public_key.clone(),
                            did_for_public_key(&identity.public_key),
                        ))
                    })
                    .transpose()?;
                let entries = metas
                    .into_iter()
                    .filter(|meta| match &verifier {
                        None => true,
                        Some((public_key, did)) => {
                            memory_attests_identity(&repo, &meta.path, public_key, did)
                        }
                    })
                    .map(|meta| {
                        let id = meta
                            .path
                            .trim_end_matches(".md")
                            .trim_start_matches("memory/")
                            .to_string();
                        VaultEntry {
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
                        }
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
        _request: Request<InitVaultRequest>,
    ) -> Result<Response<InitVaultResponse>, Status> {
        Err(Status::unimplemented("InitVault lands with its slice"))
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
        _request: Request<ExportVaultRequest>,
    ) -> Result<Response<ExportVaultResponse>, Status> {
        Err(Status::unimplemented("ExportVault lands with its slice"))
    }

    async fn delete_vault_entity(
        &self,
        _request: Request<DeleteVaultEntityRequest>,
    ) -> Result<Response<DeleteVaultEntityResponse>, Status> {
        Err(Status::unimplemented(
            "DeleteVaultEntity lands with its slice",
        ))
    }

    async fn link_vault_entities(
        &self,
        _request: Request<LinkVaultEntitiesRequest>,
    ) -> Result<Response<LinkVaultEntitiesResponse>, Status> {
        Err(Status::unimplemented(
            "LinkVaultEntities lands with its slice",
        ))
    }
}

// ---------------------------------------------------------------------------
// AttestationService
// ---------------------------------------------------------------------------

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
        _request: Request<PrepareAttestationRequest>,
    ) -> Result<Response<PrepareAttestationResponse>, Status> {
        Err(Status::unimplemented(
            "PrepareAttestation (external signing) lands with its slice",
        ))
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

        let response = tokio::task::spawn_blocking(move || match target_kind {
            TargetKind::IntentId(intent_id) => {
                let repo = handle.repository()?;
                let entry = repo
                    .vault_intent_show(&intent_id)
                    .map_err(repository_error)?;
                let frontmatter = parse_frontmatter(&entry)?;
                let body = body_of(&entry);
                let material = load_identity(if identity_name.is_empty() {
                    None
                } else {
                    Some(&identity_name)
                })?;
                let node: CanonicalNode =
                    lift_and_attest(&frontmatter, &body, &material.identity, &material.keypair)
                        .map_err(|error| {
                            domain_status(ErrorCode::InvalidArgument, format!("attest: {error}"))
                        })?;
                let report = validate_intent(&node);
                if !report.conforms {
                    return Err(domain_status(
                        ErrorCode::PreconditionFailed,
                        format!("attested intent does not conform: {}", report),
                    ));
                }
                verify(&node, &material.keypair.public).map_err(|error| {
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
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
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
                let pool = 5000usize;
                let nodes = repo
                    .vault_kg_search(&search.query, pool, Some(pool))
                    .map_err(repository_error)?;
                let nodes: Vec<_> = nodes
                    .into_iter()
                    .take(limit)
                    .map(|node| KgNode {
                        id: node.id.clone(),
                        node_type: node.kind.clone(),
                        name: Some(node.label.clone()),
                        data: node.summary.clone().map(String::into_bytes),
                        score: None,
                    })
                    .collect();
                Ok::<_, Status>(QueryGraphResponse {
                    nodes,
                    edges: Vec::new(),
                    hits: Vec::new(),
                    answer: None,
                    plan_result: None,
                    snapshot: None,
                })
            }
            QueryOneof::Neighbors(neighbors) => {
                let repo = handle.repository()?;
                let (nodes, edges) =
                    crate::daemon::services_query::neighbors_impl(&repo, &neighbors.node_id, 1)?;
                Ok(QueryGraphResponse {
                    nodes,
                    edges,
                    hits: Vec::new(),
                    answer: None,
                    plan_result: None,
                    snapshot: None,
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
                    })
                    .collect();
                Ok(QueryGraphResponse {
                    nodes,
                    edges: Vec::new(),
                    hits: Vec::new(),
                    answer: None,
                    plan_result: None,
                    snapshot: None,
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
            let repo = handle.repository()?;
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
                            repo.kg_enrich_change(&atomic_core::types::Merkle(bytes))
                                .map_err(repository_error)?;
                            total += 1;
                        }
                        return Ok::<_, Status>(MaintainKnowledgeGraphResponse {
                            processed: total,
                            meta: response_meta(&meta),
                        });
                    }
                    // The CLI --rebuild flag rides the scope field ("rebuild").
                    if request.scope.as_deref() == Some("rebuild") {
                        repo.kg_clear_derived().map_err(repository_error)?;
                    }
                    let mut total: u64 = 0;
                    total += repo.kg_enrich_views().map_err(repository_error)? as u64;
                    total += repo.kg_enrich_files().map_err(repository_error)? as u64;
                    total += repo.kg_enrich_modules().map_err(repository_error)? as u64;
                    total += repo.kg_enrich_changes().map_err(repository_error)? as u64;
                    total += repo.kg_enrich_entities().map_err(repository_error)? as u64;
                    total += repo.kg_enrich_includes().map_err(repository_error)? as u64;
                    total += repo.kg_enrich_calls().map_err(repository_error)? as u64;
                    total
                }
                KnowledgeMaintainAction::Reindex => {
                    repo.vault_reindex_kg().map_err(repository_error)? as u64
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
        _request: Request<ForkSessionRequest>,
    ) -> Result<Response<ForkSessionResponse>, Status> {
        Err(Status::unimplemented("ForkSession lands with its slice"))
    }

    async fn rebuild_session_index(
        &self,
        _request: Request<RebuildSessionIndexRequest>,
    ) -> Result<Response<RebuildSessionIndexResponse>, Status> {
        Err(Status::unimplemented(
            "RebuildSessionIndex lands with its slice",
        ))
    }

    async fn get_provenance(
        &self,
        _request: Request<GetProvenanceRequest>,
    ) -> Result<Response<GetProvenanceResponse>, Status> {
        Err(Status::unimplemented("GetProvenance lands with its slice"))
    }

    async fn export_provenance(
        &self,
        _request: Request<ExportProvenanceRequest>,
    ) -> Result<Response<ExportProvenanceResponse>, Status> {
        Err(Status::unimplemented(
            "ExportProvenance lands with its slice",
        ))
    }
}
