//! Knowledge graph extraction and query operations for the vault.
//!
//! Extracts `KgNode`s and `KgEdge`s from vault entries (goals, intents,
//! memory, skills) and stores them in the pristine KG tables.

use super::*;
use crate::content_search::{has_content_index, search_content, ContentSearchOptions};
use atomic_core::pristine::ontology::{edge_kind, predicate};
use atomic_core::pristine::vault::{KgEdge, KgNode, KgSubgraph, VaultEntry, VaultEntryType};
use atomic_core::pristine::{KgMutTxnT, KgTxnT};

impl Repository {
    /// Initialize the knowledge graph tables.
    pub fn init_kg(&self) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.init_kg()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    /// Extract KG nodes and edges from a vault entry.
    ///
    /// Returns `(Vec<KgNode>, Vec<KgEdge>)`. Does **not** store them —
    /// call `vault_store_kg` for persistence.
    pub fn vault_extract_kg(
        &self,
        path: &str,
        entry: &VaultEntry,
    ) -> Result<(Vec<KgNode>, Vec<KgEdge>), RepositoryError> {
        let mut nodes: Vec<KgNode> = Vec::new();
        let mut edges: Vec<KgEdge> = Vec::new();

        // Determine the node ID and kind from the entry type and path
        let subject = entry_subject(path, entry.entry_type);

        let kind = match entry.entry_type {
            VaultEntryType::Session => "goal",
            VaultEntryType::Intent => "intent",
            VaultEntryType::Memory => "memory",
            VaultEntryType::Skill => "skill",
            VaultEntryType::ToolResult => "tool_result",
            VaultEntryType::Scratch => return Ok((nodes, edges)), // no KG data for scratch
        };

        // Derive a label from the subject (the part after the colon)
        let label = subject.split_once(':').map(|(_, l)| l).unwrap_or(&subject);

        let mut node = KgNode::new(&subject, kind, label, "vault");

        // Extract from frontmatter
        if let Ok(fm) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
            &entry.frontmatter_json,
        ) {
            extract_frontmatter_kg(
                &subject,
                entry.entry_type,
                &fm,
                &mut node,
                &mut nodes,
                &mut edges,
            );
        }

        // Extract from content
        let content = String::from_utf8_lossy(&entry.content_bytes);
        extract_content_edges(&subject, entry.entry_type, &content, &mut edges);

        nodes.push(node);
        Ok((nodes, edges))
    }

    /// Store KG nodes and edges for a vault entry, replacing any existing
    /// nodes/edges derived from that path.
    pub fn vault_store_kg(
        &self,
        path: &str,
        nodes: &[KgNode],
        edges: &[KgEdge],
    ) -> Result<usize, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Delete old data keyed by the path's primary node ID
        let subject = path_to_subject(path);
        let _ = txn
            .del_kg_edges_for_node(&subject)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let _ = txn
            .del_kg_node(&subject)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Store new nodes
        for node in nodes {
            txn.upsert_kg_node(node)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Store new edges
        for edge in edges {
            txn.upsert_kg_edge(edge)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(nodes.len() + edges.len())
    }

    /// Extract and store KG data for a vault entry in one step.
    pub fn vault_index_kg(&self, path: &str) -> Result<usize, RepositoryError> {
        let entry = self
            .vault_retrieve(path)?
            .ok_or_else(|| RepositoryError::FileNotFound {
                path: std::path::PathBuf::from(path),
            })?;
        let (nodes, edges) = self.vault_extract_kg(path, &entry)?;
        self.vault_store_kg(path, &nodes, &edges)
    }

    /// Re-index all vault entries into the knowledge graph.
    pub fn vault_reindex_kg(&self) -> Result<usize, RepositoryError> {
        let entries = self.vault_list("", None)?;
        let mut total = 0;
        for meta in &entries {
            match self.vault_index_kg(&meta.path) {
                Ok(count) => total += count,
                Err(e) => {
                    log::warn!("Failed to index KG for {}: {}", meta.path, e);
                }
            }
        }
        Ok(total)
    }

    /// Full-text search over knowledge graph nodes.
    ///
    /// `pool` controls the candidate heap size — how many nodes are scored
    /// before the top `limit` are selected. A larger pool lets lower-scored
    /// node kinds (e.g. changes) survive alongside higher-scored ones
    /// (e.g. files with content matches). When `None`, defaults to `limit`.
    pub fn vault_kg_search(
        &self,
        query: &str,
        limit: usize,
        pool: Option<usize>,
    ) -> Result<Vec<KgNode>, RepositoryError> {
        use std::cmp::Reverse;
        use std::collections::{BinaryHeap, HashMap, HashSet};

        let pool_size = pool.unwrap_or(limit).max(limit);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // ── Phase 1: Collect ALL matching node IDs (cheap — no node fetching) ──

        // KG FTS: every node ID that matches any query token, with hit counts.
        let kg_matches: HashMap<String, usize> = txn
            .kg_fts_match_ids(query)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .into_iter()
            .collect();

        // Content search: file-level matches from syntext, grouped by path.
        let mut content_counts: HashMap<String, usize> = HashMap::new();
        if has_content_index(self.root()) {
            let opts = ContentSearchOptions {
                max_results: Some(2000),
                ..Default::default()
            };
            if let Ok(content_results) = search_content(self.root(), query, opts) {
                for m in &content_results.matches {
                    *content_counts.entry(m.path.clone()).or_insert(0) += 1;
                }
            }
        }

        // ── Phase 2: Stream candidates through a bounded min-heap ──────────
        //
        // As each candidate arrives, score it and bubble it into the heap.
        // The heap holds at most `pool_size` items — weakest score at the top.
        // If a new candidate beats the weakest, it replaces it.
        // We trim to `limit` in Phase 3.

        // Min-heap: Reverse so BinaryHeap (max-heap) gives us min-score at top.
        // Tuple: (score, node_id, content_match_count)
        let mut heap: BinaryHeap<Reverse<(u64, String, usize)>> =
            BinaryHeap::with_capacity(pool_size + 1);
        let mut seen: HashSet<String> = HashSet::new();

        // Helper: push a candidate into the bounded heap
        let push_candidate =
            |id: String,
             kg_hits: usize,
             content_matches: usize,
             heap: &mut BinaryHeap<Reverse<(u64, String, usize)>>| {
                let score = id_rank_score(&id, kg_hits, content_matches);
                if heap.len() < pool_size {
                    heap.push(Reverse((score, id, content_matches)));
                } else if let Some(&Reverse((min_score, _, _))) = heap.peek() {
                    if score > min_score {
                        heap.pop();
                        heap.push(Reverse((score, id, content_matches)));
                    }
                }
            };

        // Stream KG matches through the heap
        for (id, kg_hits) in &kg_matches {
            let cm = id
                .strip_prefix("file:")
                .and_then(|p| content_counts.get(p))
                .copied()
                .unwrap_or(0);
            seen.insert(id.clone());
            push_candidate(id.clone(), *kg_hits, cm, &mut heap);
        }

        // Stream content-only files (not already seen from KG)
        for (path, &count) in &content_counts {
            let file_id = format!("file:{}", path);
            if seen.contains(&file_id) {
                continue;
            }
            push_candidate(file_id, 0, count, &mut heap);
        }

        // ── Phase 3: Diversity-aware selection ─────────────────────────────
        //
        // When pool_size > limit the heap holds more candidates than we
        // need.  Pure top-N by score would return only the dominant kind
        // (usually files with content matches).  Instead, reserve a
        // minimum number of slots per node kind, then fill the rest by
        // overall score.  This guarantees that changes, entities, and
        // other kinds appear in results when they match the query.

        let mut all_candidates: Vec<(u64, String, usize)> =
            heap.into_iter().map(|Reverse(entry)| entry).collect();
        all_candidates.sort_by_key(|entry| std::cmp::Reverse(entry.0));

        let ranked = if pool_size > limit {
            // Diversity mode: reserve slots per kind.
            let min_per_kind: usize = 2;

            // Group by kind (inferred from ID prefix)
            let mut by_kind: HashMap<String, Vec<(u64, String, usize)>> = HashMap::new();
            for entry in all_candidates {
                let kind = entry.1.split(':').next().unwrap_or("other").to_string();
                by_kind.entry(kind).or_default().push(entry);
            }

            // First pass: take up to min_per_kind from each kind (already
            // sorted by score within each group since all_candidates was sorted).
            let mut selected: Vec<(u64, String, usize)> = Vec::with_capacity(limit);
            let mut remaining: Vec<(u64, String, usize)> = Vec::new();
            for (_kind, mut entries) in by_kind {
                let reserved: Vec<_> = entries.drain(..entries.len().min(min_per_kind)).collect();
                selected.extend(reserved);
                remaining.extend(entries);
            }

            // Second pass: fill remaining slots from leftover candidates by score
            if selected.len() < limit {
                remaining.sort_by_key(|e| std::cmp::Reverse(e.0));
                selected.extend(remaining.into_iter().take(limit - selected.len()));
            }

            // Final sort by score for consistent output order
            selected.sort_by_key(|e| std::cmp::Reverse(e.0));
            selected.truncate(limit);
            selected
        } else {
            // No diversity — simple top-N by score (original behavior)
            all_candidates.truncate(limit);
            all_candidates
        };

        // ── Phase 4: Fetch full nodes only for the top N ───────────────────

        let mut results: Vec<KgNode> = Vec::with_capacity(ranked.len());
        for (_score, id, content_match_count) in &ranked {
            let node = match txn.get_kg_node(id) {
                Ok(Some(mut n)) => {
                    if *content_match_count > 0 {
                        let md = n.metadata.get_or_insert_with(|| serde_json::json!({}));
                        if let Some(obj) = md.as_object_mut() {
                            obj.insert(
                                "content_matches".to_string(),
                                serde_json::json!(content_match_count),
                            );
                        }
                    }
                    n
                }
                _ => {
                    // Node not in KG — create a stub (content-only file)
                    let label = id
                        .strip_prefix("file:")
                        .and_then(|p| p.rsplit_once('/').map(|(_, name)| name))
                        .unwrap_or(id);
                    let mut n = KgNode::new(id, "file", label, "content_search");
                    if *content_match_count > 0 {
                        n = n.with_metadata(
                            serde_json::json!({"content_matches": content_match_count}),
                        );
                    }
                    n
                }
            };
            results.push(node);
        }

        Ok(results)
    }
}

/// Compute a ranking score from a node ID and match counts.
///
/// Higher score = more relevant.  Works on IDs only (no node fetching needed).
/// Combines: node kind weight, path tier, KG hit count, content match count.
fn id_rank_score(id: &str, kg_hits: usize, content_matches: usize) -> u64 {
    // Base score by node kind (inferred from ID prefix).
    //
    // Changes score the same as files: a change whose commit message
    // matches the query is equally relevant as a file whose name does.
    // Content matches (syntext hits) still boost files above changes
    // when the query appears heavily inside file bodies.
    let kind_score: u64 = if id.starts_with("module:") {
        500
    } else if id.starts_with("file:") {
        400
    } else if id.starts_with("entity:") {
        300
    } else if id.starts_with("change:") {
        400
    } else {
        200
    };

    // Extract path from the ID for tier ranking
    let path = extract_path_from_id(id);
    let tier_penalty: u64 = match path_tier(path) {
        0 => 0,   // src/ — no penalty
        1 => 20,  // other code files
        2 => 80,  // test files
        3 => 120, // docs
        4 => 200, // config/build/CI
        _ => 150,
    };

    // KG hit bonus: multi-token matches are more relevant
    let kg_bonus: u64 = (kg_hits as u64).saturating_sub(1) * 50;

    // Content match bonus: files with many content hits are more relevant
    let content_bonus: u64 = (content_matches as u64).min(100);

    kind_score.saturating_sub(tier_penalty) + kg_bonus + content_bonus
}

/// Extract a file path from a node ID for ranking purposes.
fn extract_path_from_id(id: &str) -> &str {
    if let Some(path) = id.strip_prefix("file:") {
        return path;
    }
    if let Some(path) = id.strip_prefix("module:") {
        return path;
    }
    // entity:file:name:line — extract the file portion
    if let Some(rest) = id.strip_prefix("entity:") {
        // e.g., "src/mongo/db/repl/repl.cpp:ReplicationCoordinator:42"
        // Find the second-to-last colon to get the file path
        let parts: Vec<&str> = rest.rsplitn(3, ':').collect();
        if parts.len() == 3 {
            return parts[2];
        }
    }
    id
}

/// Assign a ranking tier to a file path.
///
/// Lower tier = higher priority (less penalty).
fn path_tier(path: &str) -> u8 {
    // Tier 0: primary source directories
    let source_prefixes = ["src/", "lib/", "pkg/", "internal/", "cmd/", "app/"];
    for prefix in &source_prefixes {
        if path.starts_with(prefix) {
            if is_test_path(path) {
                return 2;
            }
            return 0;
        }
    }

    // Tier 1: other code files
    let code_extensions = [
        ".rs", ".go", ".py", ".ts", ".js", ".cpp", ".cc", ".c", ".h", ".hpp", ".java", ".kt",
        ".swift", ".rb", ".cs",
    ];
    if code_extensions.iter().any(|ext| path.ends_with(ext)) {
        if is_test_path(path) {
            return 2;
        }
        return 1;
    }

    // Tier 2: test paths
    if is_test_path(path) {
        return 2;
    }

    // Tier 3: docs
    if path.ends_with(".md") || path.starts_with("docs/") || path.starts_with("doc/") {
        return 3;
    }

    // Tier 4: build scripts, config, CI, generated files
    let low_priority = [
        "buildscripts/",
        "build/",
        ".github/",
        "ci/",
        "scripts/",
        "debian/",
        "rpm/",
        "packaging/",
        "vendor/",
        "third_party/",
        "node_modules/",
        "target/",
    ];
    if low_priority.iter().any(|p| path.starts_with(p)) {
        return 4;
    }
    if path.ends_with(".yml")
        || path.ends_with(".yaml")
        || path.ends_with(".toml")
        || path.ends_with(".json")
        || path.ends_with(".xml")
        || path.ends_with(".cfg")
    {
        return 4;
    }

    3
}

/// Heuristic: is this path a test file?
fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("_test.")
        || lower.contains("_test_")
        || lower.contains(".test.")
        || lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.starts_with("jstests/")
        || lower.starts_with("testdata/")
}

impl Repository {
    /// Get the neighborhood subgraph around a node.
    pub fn vault_kg_neighbors(
        &self,
        node_id: &str,
        depth: u8,
    ) -> Result<KgSubgraph, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.kg_neighbors(node_id, depth)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get a single KG node by ID.
    pub fn vault_kg_node(&self, id: &str) -> Result<Option<KgNode>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.get_kg_node(id)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count total KG nodes.
    pub fn vault_kg_node_count(&self) -> Result<usize, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.count_kg_nodes()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count total KG edges.
    pub fn vault_kg_edge_count(&self) -> Result<usize, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.count_kg_edges()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }
}

// ── Extraction helpers ──────────────────────────────────────────

/// Derive a subject URI from a vault path and entry type.
fn entry_subject(path: &str, entry_type: VaultEntryType) -> String {
    match entry_type {
        VaultEntryType::Session => {
            // goals/swift-meadow-a3f2/_goal.md -> goal:swift-meadow-a3f2
            let name = path
                .strip_prefix("goals/")
                .and_then(|s| s.strip_suffix("/_goal.md"))
                .unwrap_or(path);
            format!("goal:{}", name)
        }
        VaultEntryType::Intent => {
            // intents/pimo-1/intent.md -> intent:PIMO-1
            let id = path
                .strip_prefix("intents/")
                .and_then(|s| s.strip_suffix("/intent.md"))
                .unwrap_or(path)
                .to_uppercase();
            format!("intent:{}", id)
        }
        VaultEntryType::Memory => {
            // memory/architecture.md -> memory:architecture
            let name = path
                .strip_prefix("memory/")
                .and_then(|s| s.strip_suffix(".md"))
                .unwrap_or(path);
            format!("memory:{}", name)
        }
        VaultEntryType::Skill => {
            let name = path
                .strip_prefix("skills/")
                .and_then(|s| s.strip_suffix(".md"))
                .unwrap_or(path);
            format!("skill:{}", name)
        }
        VaultEntryType::ToolResult => {
            format!("tool:{}", path.replace('/', ":"))
        }
        VaultEntryType::Scratch => {
            format!("scratch:{}", path.replace('/', ":"))
        }
    }
}

/// Convert a vault path to a subject URI (simplified).
fn path_to_subject(path: &str) -> String {
    if path.starts_with("goals/") {
        entry_subject(path, VaultEntryType::Session)
    } else if path.starts_with("intents/") {
        entry_subject(path, VaultEntryType::Intent)
    } else if path.starts_with("memory/") {
        entry_subject(path, VaultEntryType::Memory)
    } else if path.starts_with("skills/") {
        entry_subject(path, VaultEntryType::Skill)
    } else {
        format!("vault:{}", path.replace('/', ":"))
    }
}

/// Extract KG edges (and populate node summary) from frontmatter fields.
fn extract_frontmatter_kg(
    subject: &str,
    entry_type: VaultEntryType,
    fm: &serde_json::Map<String, serde_json::Value>,
    node: &mut KgNode,
    nodes: &mut Vec<KgNode>,
    edges: &mut Vec<KgEdge>,
) {
    // Common fields → edges
    if let Some(s) = fm.get("developer").and_then(|v| v.as_str()) {
        edges.push(KgEdge::new(
            subject,
            format!("identity:{}", s),
            edge_kind::AUTHORED_BY,
        ));
    }
    if let Some(s) = fm.get("creator").and_then(|v| v.as_str()) {
        edges.push(KgEdge::new(
            subject,
            format!("identity:{}", s),
            edge_kind::AUTHORED_BY,
        ));
    }

    // description / title → node summary
    if let Some(s) = fm.get("description").and_then(|v| v.as_str()) {
        node.summary = Some(s.to_string());
    }
    if let Some(s) = fm.get("title").and_then(|v| v.as_str()) {
        if node.summary.is_none() {
            node.summary = Some(s.to_string());
        }
    }

    // Entry-type-specific fields
    match entry_type {
        VaultEntryType::Session => {
            if let Some(s) = fm.get("intent").and_then(|v| v.as_str()) {
                edges.push(KgEdge::new(
                    subject,
                    format!("intent:{}", s),
                    edge_kind::LINKED_INTENT,
                ));
            }
            if let Some(s) = fm.get("status").and_then(|v| v.as_str()) {
                node.metadata = Some(serde_json::json!({ "status": s }));
            }
        }
        VaultEntryType::Intent => {
            if let Some(s) = fm.get("assignee").and_then(|v| v.as_str()) {
                edges.push(KgEdge::new(
                    subject,
                    format!("identity:{}", s),
                    edge_kind::ASSIGNED_TO,
                ));
            }
            if let Some(s) = fm.get("status").and_then(|v| v.as_str()) {
                let md = node.metadata.get_or_insert_with(|| serde_json::json!({}));
                if let Some(obj) = md.as_object_mut() {
                    obj.insert("status".to_string(), serde_json::json!(s));
                }
            }
            if let Some(s) = fm.get("priority").and_then(|v| v.as_str()) {
                let md = node.metadata.get_or_insert_with(|| serde_json::json!({}));
                if let Some(obj) = md.as_object_mut() {
                    obj.insert("priority".to_string(), serde_json::json!(s));
                }
            }
            // blocked_by array — write both directions for symmetric traversal
            if let Some(arr) = fm.get("blocked_by").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        edges.push(KgEdge::new(
                            subject,
                            format!("intent:{}", s),
                            edge_kind::BLOCKED_BY,
                        ));
                        // Symmetric: the blocker also BLOCKS this intent
                        edges.push(KgEdge::new(
                            format!("intent:{}", s),
                            subject,
                            edge_kind::BLOCKS,
                        ));
                    }
                }
            }
            // labels array -> HAS_LABEL edges + concept nodes
            if let Some(arr) = fm.get("labels").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        edges.push(KgEdge::new(
                            subject,
                            format!("concept:{}", s),
                            edge_kind::HAS_LABEL,
                        ));
                        // Ensure the concept node exists so it is searchable
                        nodes.push(KgNode::new(format!("concept:{}", s), "concept", s, "vault"));
                    }
                }
            }
            // goals/sessions array
            if let Some(arr) = fm.get("goals").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        edges.push(KgEdge::new(
                            subject,
                            format!("goal:{}", s),
                            edge_kind::LINKED_GOAL,
                        ));
                    }
                }
            }
        }
        VaultEntryType::Memory => {
            if let Some(s) = fm.get("name").and_then(|v| v.as_str()) {
                if node.summary.is_none() {
                    node.summary = Some(s.to_string());
                }
            }
        }
        VaultEntryType::Skill => {
            if let Some(s) = fm.get("name").and_then(|v| v.as_str()) {
                if node.summary.is_none() {
                    node.summary = Some(s.to_string());
                }
            }
        }
        VaultEntryType::ToolResult => {
            // ToolResult files always live at goals/{goal-name}/toolu_*.md.
            // Derive the parent goal from the subject: "tool:goals:{goal}:{file}"
            if let Some(rest) = subject.strip_prefix("tool:goals:") {
                if let Some((goal_name, _)) = rest.split_once(':') {
                    edges.push(KgEdge::new(
                        subject,
                        format!("goal:{}", goal_name),
                        predicate::WAS_ASSOCIATED_WITH,
                    ));
                }
            }
        }
        _ => {}
    }
}

/// Extract edges from content text (wiki-links, file paths).
fn extract_content_edges(
    subject: &str,
    entry_type: VaultEntryType,
    content: &str,
    edges: &mut Vec<KgEdge>,
) {
    // Extract [[wiki-links]] — always REFERENCES regardless of entry type
    let mut pos = 0;
    while let Some(start) = content[pos..].find("[[") {
        let abs_start = pos + start + 2;
        if let Some(end) = content[abs_start..].find("]]") {
            let link = &content[abs_start..abs_start + end];
            if !link.is_empty() && !link.contains('\n') {
                edges.push(KgEdge::new(
                    subject,
                    format!("memory:{}", link),
                    edge_kind::REFERENCES,
                ));
            }
            pos = abs_start + end + 2;
        } else {
            break;
        }
    }

    // File paths: ToolResult content represents files the agent actually read/used,
    // so emit USED edges.  All other entry types emit REFERENCES edges.
    let file_edge_kind = if entry_type == VaultEntryType::ToolResult {
        predicate::USED
    } else {
        edge_kind::REFERENCES
    };

    for word in content.split_whitespace() {
        let clean = word.trim_matches(|c: char| {
            c == '`' || c == '"' || c == '\'' || c == '(' || c == ')' || c == ','
        });
        if looks_like_file_path(clean) {
            edges.push(KgEdge::new(
                subject,
                format!("file:{}", clean),
                file_edge_kind,
            ));
        }
    }
}

/// Heuristic: does this string look like a file path?
fn looks_like_file_path(s: &str) -> bool {
    if s.len() < 4 || s.len() > 200 {
        return false;
    }
    // Must contain a slash and a dot (extension)
    if !s.contains('/') || !s.contains('.') {
        return false;
    }
    // Common code file extensions
    let extensions = [
        ".rs", ".ts", ".js", ".py", ".go", ".java", ".md", ".toml", ".yaml", ".yml", ".json",
        ".html", ".css",
    ];
    extensions.iter().any(|ext| s.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_extract_kg_from_goal() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Session,
            b"# Goal\nLooking at src/auth.rs\n".to_vec(),
            r#"{"developer":"alice","intent":"PIMO-1","status":"active"}"#.to_string(),
            "2025-01-01T00:00:00Z".to_string(),
        );

        let (nodes, edges) = repo
            .vault_extract_kg("goals/test-goal/_goal.md", &entry)
            .unwrap();

        // Should have a goal node
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, "goal");
        assert_eq!(nodes[0].id, "goal:test-goal");
        assert_eq!(nodes[0].source, "vault");

        // Should have edges for developer, intent, file reference
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::AUTHORED_BY && e.to_id == "identity:alice"),
            "Missing AUTHORED_BY edge"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::LINKED_INTENT && e.to_id == "intent:PIMO-1"),
            "Missing LINKED_INTENT edge"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id.contains("auth.rs")),
            "Missing REFERENCES edge for file path"
        );
    }

    #[test]
    fn test_extract_kg_from_intent() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Intent,
            b"# Fix auth\n\nSee [[architecture]]\n".to_vec(),
            r#"{"title":"Fix auth","status":"in-progress","priority":"high","assignee":"bob","labels":["auth","security"]}"#.to_string(),
            "2025-01-01T00:00:00Z".to_string(),
        );

        let (nodes, edges) = repo
            .vault_extract_kg("intents/pimo-1/intent.md", &entry)
            .unwrap();

        // 1 intent node + 1 concept node per label ("auth", "security")
        assert_eq!(nodes.len(), 3);
        let intent_node = nodes.iter().find(|n| n.kind == "intent").unwrap();
        assert_eq!(intent_node.id, "intent:PIMO-1");
        assert_eq!(intent_node.summary.as_deref(), Some("Fix auth"));
        assert!(nodes
            .iter()
            .any(|n| n.kind == "concept" && n.id == "concept:auth"));
        assert!(nodes
            .iter()
            .any(|n| n.kind == "concept" && n.id == "concept:security"));

        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::ASSIGNED_TO && e.to_id == "identity:bob"),
            "Missing ASSIGNED_TO edge"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::HAS_LABEL && e.to_id == "concept:auth"),
            "Missing HAS_LABEL edge for auth"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::HAS_LABEL && e.to_id == "concept:security"),
            "Missing HAS_LABEL edge for security"
        );
        // Wiki-link → REFERENCES edge
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id == "memory:architecture"),
            "Missing REFERENCES edge for wiki-link"
        );
    }

    #[test]
    fn test_kg_search() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store and index
        repo.vault_store(
            "memory/architecture.md",
            VaultEntryType::Memory,
            b"# Architecture\n".to_vec(),
            r#"{"name":"architecture"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("memory/architecture.md").unwrap();

        // FTS search
        let results = repo.vault_kg_search("architecture", 10, None).unwrap();
        assert!(!results.is_empty(), "FTS search should return results");
        assert_eq!(results[0].kind, "memory");
    }

    #[test]
    fn test_kg_neighbors() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store a goal with edges
        repo.vault_store(
            "goals/abc/_goal.md",
            VaultEntryType::Session,
            b"# Goal\n".to_vec(),
            r#"{"developer":"alice","intent":"PIMO-1"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("goals/abc/_goal.md").unwrap();

        let sg = repo.vault_kg_neighbors("goal:abc", 1).unwrap();
        assert!(!sg.is_empty(), "Neighborhood should not be empty");
    }

    #[test]
    fn test_kg_counts() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/test.md",
            VaultEntryType::Memory,
            b"# Test\n".to_vec(),
            r#"{"name":"test"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("memory/test.md").unwrap();

        assert!(repo.vault_kg_node_count().unwrap() > 0);
    }

    #[test]
    fn test_kg_store_replaces_old_data() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let nodes = vec![KgNode::new("goal:abc", "goal", "abc", "vault")];
        let edges = vec![KgEdge::new(
            "goal:abc",
            "file:src/main.rs",
            edge_kind::REFERENCES,
        )];

        let count = repo
            .vault_store_kg("goals/abc/_goal.md", &nodes, &edges)
            .unwrap();
        assert_eq!(count, 2);

        // Store again with different edges — old data should be replaced
        let edges2 = vec![KgEdge::new(
            "goal:abc",
            "file:src/lib.rs",
            edge_kind::REFERENCES,
        )];
        let count2 = repo
            .vault_store_kg("goals/abc/_goal.md", &nodes, &edges2)
            .unwrap();
        assert_eq!(count2, 2);

        // The node should still exist
        let node = repo.vault_kg_node("goal:abc").unwrap();
        assert!(node.is_some());
    }

    #[test]
    fn test_looks_like_file_path() {
        assert!(looks_like_file_path("src/auth.rs"));
        assert!(looks_like_file_path("crates/pi-agent/src/lib.rs"));
        assert!(!looks_like_file_path("hello"));
        assert!(!looks_like_file_path("http://example.com"));
        assert!(!looks_like_file_path("a"));
    }

    #[test]
    fn test_entry_subject() {
        assert_eq!(
            entry_subject("goals/abc/_goal.md", VaultEntryType::Session),
            "goal:abc"
        );
        assert_eq!(
            entry_subject("intents/pimo-1/intent.md", VaultEntryType::Intent),
            "intent:PIMO-1"
        );
        assert_eq!(
            entry_subject("memory/architecture.md", VaultEntryType::Memory),
            "memory:architecture"
        );
        assert_eq!(
            entry_subject("skills/run-tests.md", VaultEntryType::Skill),
            "skill:run-tests"
        );
    }

    #[test]
    fn test_path_to_subject() {
        assert_eq!(path_to_subject("goals/abc/_goal.md"), "goal:abc");
        assert_eq!(path_to_subject("intents/pimo-1/intent.md"), "intent:PIMO-1");
        assert_eq!(
            path_to_subject("memory/architecture.md"),
            "memory:architecture"
        );
        assert_eq!(path_to_subject("skills/run-tests.md"), "skill:run-tests");
        assert_eq!(
            path_to_subject("scratch/notes.md"),
            "vault:scratch:notes.md"
        );
    }

    #[test]
    fn test_extract_content_edges_wiki_links() {
        let mut edges = Vec::new();
        extract_content_edges(
            "goal:test",
            VaultEntryType::Session,
            "Check [[architecture]] and [[auth-design]] for details.",
            &mut edges,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id == "memory:architecture"),
            "Missing wiki-link edge for architecture"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id == "memory:auth-design"),
            "Missing wiki-link edge for auth-design"
        );
    }

    #[test]
    fn test_extract_content_edges_file_paths() {
        let mut edges = Vec::new();
        extract_content_edges(
            "goal:test",
            VaultEntryType::Session,
            "Modified `src/main.rs` and crates/core/src/lib.rs today.",
            &mut edges,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id == "file:src/main.rs"),
            "Missing file path edge for src/main.rs"
        );
        assert!(
            edges.iter().any(
                |e| e.kind == edge_kind::REFERENCES && e.to_id == "file:crates/core/src/lib.rs"
            ),
            "Missing file path edge for crates/core/src/lib.rs"
        );
    }

    #[test]
    fn test_extract_scratch_returns_empty() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Scratch,
            b"just some scratch notes".to_vec(),
            "{}".to_string(),
            "2025-01-01T00:00:00Z".to_string(),
        );

        let (nodes, edges) = repo.vault_extract_kg("scratch/temp.md", &entry).unwrap();
        assert!(nodes.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn test_frontmatter_intent_blocked_by() {
        let mut node = KgNode::new("intent:PIMO-1", "intent", "PIMO-1", "vault");
        let mut edges = Vec::new();
        let mut fm = serde_json::Map::new();
        fm.insert(
            "blocked_by".to_string(),
            serde_json::json!(["PIMO-2", "PIMO-3"]),
        );
        let mut nodes = Vec::new();
        extract_frontmatter_kg(
            "intent:PIMO-1",
            VaultEntryType::Intent,
            &fm,
            &mut node,
            &mut nodes,
            &mut edges,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::BLOCKED_BY && e.to_id == "intent:PIMO-2"),
            "Missing BLOCKED_BY edge for PIMO-2"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::BLOCKED_BY && e.to_id == "intent:PIMO-3"),
            "Missing BLOCKED_BY edge for PIMO-3"
        );
        // Reverse BLOCKS edges should also be present
        assert!(
            edges.iter().any(|e| e.kind == edge_kind::BLOCKS
                && e.from_id == "intent:PIMO-2"
                && e.to_id == "intent:PIMO-1"),
            "Missing reverse BLOCKS edge from PIMO-2"
        );
    }

    #[test]
    fn test_frontmatter_intent_goals() {
        let mut node = KgNode::new("intent:PIMO-1", "intent", "PIMO-1", "vault");
        let mut edges = Vec::new();
        let mut fm = serde_json::Map::new();
        fm.insert(
            "goals".to_string(),
            serde_json::json!(["swift-meadow-a3f2"]),
        );
        let mut nodes = Vec::new();
        extract_frontmatter_kg(
            "intent:PIMO-1",
            VaultEntryType::Intent,
            &fm,
            &mut node,
            &mut nodes,
            &mut edges,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::LINKED_GOAL && e.to_id == "goal:swift-meadow-a3f2"),
            "Missing LINKED_GOAL edge"
        );
    }

    #[test]
    fn test_wiki_link_with_newline_ignored() {
        let mut edges = Vec::new();
        extract_content_edges(
            "goal:test",
            VaultEntryType::Session,
            "See [[bad\nlink]] here.",
            &mut edges,
        );
        assert!(
            edges.iter().all(|e| e.kind != edge_kind::REFERENCES),
            "Should not extract wiki-link with newline"
        );
    }

    #[test]
    fn test_unclosed_wiki_link_ignored() {
        let mut edges = Vec::new();
        extract_content_edges(
            "goal:test",
            VaultEntryType::Session,
            "See [[unclosed link here.",
            &mut edges,
        );
        assert!(
            edges.iter().all(|e| e.kind != edge_kind::REFERENCES),
            "Should not extract unclosed wiki-link"
        );
    }

    #[test]
    fn test_vault_index_kg() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/architecture.md",
            VaultEntryType::Memory,
            b"# Architecture\n".to_vec(),
            r#"{"name":"architecture","type":"project"}"#.to_string(),
        )
        .unwrap();

        let count = repo.vault_index_kg("memory/architecture.md").unwrap();
        assert!(count > 0, "Should have indexed at least one node");

        let nc = repo.vault_kg_node_count().unwrap();
        assert!(nc > 0, "Should have at least one node in KG");
    }

    #[test]
    fn test_memory_node_summary_from_name() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Memory,
            b"# Architecture decisions\n".to_vec(),
            r#"{"name":"architecture"}"#.to_string(),
            "2025-01-01T00:00:00Z".to_string(),
        );

        let (nodes, _edges) = repo
            .vault_extract_kg("memory/architecture.md", &entry)
            .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, "memory");
        assert_eq!(nodes[0].summary.as_deref(), Some("architecture"));
    }
}
