//! Sequence consolidation — groups raw tool nodes into Decision nodes.
//!
//! The rule-based classifier (Phase 1) produces one node per tool call. A
//! 20-tool-call turn produces 20 nodes — too noisy for the WebUI or the
//! compaction summary. This module runs after a turn completes and collapses
//! sequences of raw nodes into named `Decision` nodes that describe *what
//! the agent was trying to do* rather than listing every tool invocation.
//!
//! # Patterns Detected
//!
//! | Pattern | Decision summary | Detail |
//! |---------|-----------------|--------|
//! | N consecutive explorations | "Explored N files in {common_dir}" | `systematic_exploration` |
//! | Explore → commit | "Investigated {files} → edited {file}" | `informed_commit` |
//! | Commit → verify | "Edited {file} → verified with {cmd}" | `commit_and_verify` |
//! | Explore → commit → verify | "Investigated → fixed → verified" | `full_cycle` |
//! | Read → edit → read same → edit same | "Iterated on {file} (N attempts)" | `backtracking` |
//! | Edit → test(fail) → edit → test(pass) | "Test-driven fix for {file}" | `test_driven_iteration` |
//!
//! # Backtracking Detection
//!
//! Backtracking is when the agent revisits the same file after modifying it,
//! suggesting the first approach didn't work. This is a key signal for
//! decision quality and is surfaced in the decision node's detail as
//! `iterations` and `pattern`.
//!
//! ```text
//! read auth.rs → edit auth.rs → read auth.rs → edit auth.rs
//!                                ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
//!                                backtracking detected: 2 iterations
//! ```
//!
//! # Integration
//!
//! Called from `TurnOrchestrator::handle_turn_end` after recording:
//!
//! ```rust,ignore
//! use atomic_agent::provenance::consolidate::consolidate;
//!
//! let mut acc = load_accumulator(session_id);
//! consolidate(&mut acc);
//! acc.save(&session_dir)?;
//! ```
//!
//! Consolidation is idempotent — running it twice produces the same result.
//! Already-classified nodes (with `classified = true`) are never re-consolidated.

use super::types::{EdgeKind, GraphEdge, GraphNode, GraphStats, NodeKind};

// =============================================================================
// Public API
// =============================================================================

/// Consolidate raw tool nodes into Decision nodes.
///
/// Scans the graph for recognizable sequences of unclassified tool nodes
/// and replaces each sequence with a single `Decision` node that references
/// the originals via `consolidated_from`. The original nodes are preserved
/// in the graph (they keep their edges) — the decision node is appended
/// alongside them with new edges linking it to the sequence's context.
///
/// This is the Tier 1 (pattern-based) consolidation. It runs synchronously
/// and produces deterministic results. Tier 2 (LLM-powered naming via
/// `atomic agent explain`) can later enrich decision summaries.
///
/// Returns the number of decision nodes created.
pub fn consolidate(
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
    stats: &mut GraphStats,
    counter: &mut u64,
    session_prefix: &str,
) -> u32 {
    let mut created = 0;

    // Collect sequences of unclassified tool-derived nodes between
    // structural boundaries (goals, patches, human gates).
    let sequences = find_sequences(nodes);

    for seq in sequences {
        if seq.len() < 2 {
            continue;
        }

        // Try each pattern in priority order. First match wins.
        let decision = detect_backtracking(nodes, &seq, counter, session_prefix)
            .or_else(|| detect_test_driven_iteration(nodes, &seq, counter, session_prefix))
            .or_else(|| detect_full_cycle(nodes, &seq, counter, session_prefix))
            .or_else(|| detect_commit_and_verify(nodes, &seq, counter, session_prefix))
            .or_else(|| detect_informed_commit(nodes, &seq, counter, session_prefix))
            .or_else(|| detect_systematic_exploration(nodes, &seq, counter, session_prefix));

        if let Some((decision_node, decision_edges)) = decision {
            let decision_id = decision_node.id.clone();

            // Collect the IDs of original nodes before mutating the vec.
            let original_ids: Vec<String> = seq.iter().map(|&i| nodes[i].id.clone()).collect();

            stats.increment(NodeKind::Decision);
            stats.edge_count += decision_edges.len() as u32;
            nodes.push(decision_node);
            edges.extend(decision_edges);

            // Mark the original nodes as consolidated.
            for target_id in &original_ids {
                if let Some(node) = nodes.iter_mut().find(|n| n.id == *target_id) {
                    node.classified = true;
                    if node.consolidated_from.is_empty() {
                        node.consolidated_from = vec![decision_id.clone()];
                    }
                }
            }

            created += 1;
        }
    }

    created
}

// =============================================================================
// Sequence Finding
// =============================================================================

/// Indices of unclassified tool-derived nodes grouped into sequences
/// separated by structural boundaries (goals, patches, human gates, errors).
fn find_sequences(nodes: &[GraphNode]) -> Vec<Vec<usize>> {
    let mut sequences = Vec::new();
    let mut current = Vec::new();

    for (i, node) in nodes.iter().enumerate() {
        if node.classified {
            if current.len() >= 2 {
                sequences.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            continue;
        }

        match node.kind {
            // Structural boundaries break sequences
            NodeKind::Goal
            | NodeKind::PatchProposal
            | NodeKind::HumanGate
            | NodeKind::Todo
            | NodeKind::TodoStatusChange
            | NodeKind::PhaseTransition
            | NodeKind::Lesson
            | NodeKind::LlmResponse
            | NodeKind::HumanGateResolution => {
                if current.len() >= 2 {
                    sequences.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
            // Decision nodes are already consolidated
            NodeKind::Decision => {
                if current.len() >= 2 {
                    sequences.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
            // Tool-derived nodes accumulate into the current sequence
            NodeKind::Exploration
            | NodeKind::Commitment
            | NodeKind::Verification
            | NodeKind::Execution
            | NodeKind::Error => {
                current.push(i);
            }
        }
    }

    if current.len() >= 2 {
        sequences.push(current);
    }

    sequences
}

// =============================================================================
// Pattern Detectors
// =============================================================================

/// Detect backtracking: the agent reads/edits the same file multiple times.
///
/// Pattern: any file appears in both an exploration AND a commitment more
/// than once in the sequence, suggesting the agent revised its approach.
fn detect_backtracking(
    nodes: &[GraphNode],
    seq: &[usize],
    counter: &mut u64,
    prefix: &str,
) -> Option<(GraphNode, Vec<GraphEdge>)> {
    let mut file_edit_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();

    for &idx in seq {
        let node = &nodes[idx];
        if node.kind == NodeKind::Commitment {
            if let Some(file) = extract_file(node) {
                *file_edit_counts.entry(file).or_default() += 1;
            }
        }
    }

    // Need at least one file edited more than once
    let repeated: Vec<(&String, &u32)> = file_edit_counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .collect();

    if repeated.is_empty() {
        return None;
    }

    let max_file = repeated
        .iter()
        .max_by_key(|(_, c)| *c)
        .map(|(f, _)| f.to_string())
        .unwrap_or_default();
    let max_count = file_edit_counts.get(&max_file).copied().unwrap_or(0);

    let summary = format!(
        "Iterated on {} ({} attempts)",
        short_path(&max_file),
        max_count
    );

    let consolidated_ids: Vec<String> = seq.iter().map(|&i| nodes[i].id.clone()).collect();

    let detail = serde_json::json!({
        "pattern": "backtracking",
        "iterations": max_count,
        "file": max_file,
        "files_revisited": repeated.iter().map(|(f, c)| {
            serde_json::json!({"file": f, "edits": c})
        }).collect::<Vec<_>>(),
    });

    let id = next_id(counter, prefix);
    let timestamp = seq.last().map(|&i| nodes[i].timestamp).unwrap_or_default();

    let mut node = GraphNode::new(&id, NodeKind::Decision, timestamp, summary).with_detail(detail);
    node.classified = true;
    node.confidence = Some(0.85);
    node.consolidated_from = consolidated_ids;

    let decision_edges = build_decision_edges(nodes, seq, &id);

    Some((node, decision_edges))
}

/// Detect test-driven iteration: edit → test(fail) → edit → test(pass).
///
/// The agent edits a file, runs tests, they fail, edits again, tests pass.
fn detect_test_driven_iteration(
    nodes: &[GraphNode],
    seq: &[usize],
    counter: &mut u64,
    prefix: &str,
) -> Option<(GraphNode, Vec<GraphEdge>)> {
    // Need at least: commit, verification(fail), commit, verification(pass)
    if seq.len() < 4 {
        return None;
    }

    let mut has_failed_verification = false;
    let mut has_passed_verification = false;
    let mut has_commit_after_fail = false;
    let mut saw_fail = false;
    let mut edited_file = None;

    for &idx in seq {
        let node = &nodes[idx];
        match node.kind {
            NodeKind::Verification => {
                let passed = node
                    .detail
                    .as_ref()
                    .and_then(|d| d.get("passed"))
                    .and_then(|v| v.as_bool());

                match passed {
                    Some(false) => {
                        has_failed_verification = true;
                        saw_fail = true;
                    }
                    Some(true) if saw_fail => {
                        has_passed_verification = true;
                    }
                    _ => {}
                }
            }
            NodeKind::Commitment if saw_fail => {
                has_commit_after_fail = true;
                if edited_file.is_none() {
                    edited_file = extract_file(node);
                }
            }
            NodeKind::Commitment if !saw_fail => {
                edited_file = extract_file(node);
            }
            _ => {}
        }
    }

    if !(has_failed_verification && has_commit_after_fail && has_passed_verification) {
        return None;
    }

    let file_display = edited_file
        .as_deref()
        .map(short_path)
        .unwrap_or_else(|| "file".to_string());

    let summary = format!("Test-driven fix for {}", file_display);

    let consolidated_ids: Vec<String> = seq.iter().map(|&i| nodes[i].id.clone()).collect();

    let detail = serde_json::json!({
        "pattern": "test_driven_iteration",
        "file": edited_file,
    });

    let id = next_id(counter, prefix);
    let timestamp = seq.last().map(|&i| nodes[i].timestamp).unwrap_or_default();

    let mut node = GraphNode::new(&id, NodeKind::Decision, timestamp, summary).with_detail(detail);
    node.classified = true;
    node.confidence = Some(0.90);
    node.consolidated_from = consolidated_ids;

    let decision_edges = build_decision_edges(nodes, seq, &id);

    Some((node, decision_edges))
}

/// Detect full cycle: explore → commit → verify.
///
/// The agent reads files, makes a change, then validates it.
fn detect_full_cycle(
    nodes: &[GraphNode],
    seq: &[usize],
    counter: &mut u64,
    prefix: &str,
) -> Option<(GraphNode, Vec<GraphEdge>)> {
    let mut has_exploration = false;
    let mut has_commitment = false;
    let mut has_verification = false;
    let mut committed_file = None;
    let mut verify_cmd = None;

    for &idx in seq {
        let node = &nodes[idx];
        match node.kind {
            NodeKind::Exploration => has_exploration = true,
            NodeKind::Commitment => {
                has_commitment = true;
                if committed_file.is_none() {
                    committed_file = extract_file(node);
                }
            }
            NodeKind::Verification => {
                has_verification = true;
                if verify_cmd.is_none() {
                    verify_cmd = extract_command(node);
                }
            }
            _ => {}
        }
    }

    if !(has_exploration && has_commitment && has_verification) {
        return None;
    }

    let file_display = committed_file
        .as_deref()
        .map(short_path)
        .unwrap_or_else(|| "file".to_string());

    let cmd_display = verify_cmd
        .as_deref()
        .map(|c| truncate(c, 40))
        .unwrap_or_else(|| "tests".to_string());

    let exploration_count = seq
        .iter()
        .filter(|&&i| nodes[i].kind == NodeKind::Exploration)
        .count();

    let summary = format!(
        "Investigated ({} files) → fixed {} → verified with {}",
        exploration_count, file_display, cmd_display
    );

    let consolidated_ids: Vec<String> = seq.iter().map(|&i| nodes[i].id.clone()).collect();

    let detail = serde_json::json!({
        "pattern": "full_cycle",
        "explorations": exploration_count,
        "file": committed_file,
        "verify_command": verify_cmd,
    });

    let id = next_id(counter, prefix);
    let timestamp = seq.last().map(|&i| nodes[i].timestamp).unwrap_or_default();

    let mut node = GraphNode::new(&id, NodeKind::Decision, timestamp, summary).with_detail(detail);
    node.classified = true;
    node.confidence = Some(0.85);
    node.consolidated_from = consolidated_ids;

    let decision_edges = build_decision_edges(nodes, seq, &id);

    Some((node, decision_edges))
}

/// Detect commit-and-verify: commit → verify (without preceding exploration).
fn detect_commit_and_verify(
    nodes: &[GraphNode],
    seq: &[usize],
    counter: &mut u64,
    prefix: &str,
) -> Option<(GraphNode, Vec<GraphEdge>)> {
    let kinds: Vec<NodeKind> = seq.iter().map(|&i| nodes[i].kind).collect();

    let has_commitment = kinds.contains(&NodeKind::Commitment);
    let has_verification = kinds.contains(&NodeKind::Verification);
    let has_exploration = kinds.contains(&NodeKind::Exploration);

    if !(has_commitment && has_verification && !has_exploration) {
        return None;
    }

    let committed_file = seq
        .iter()
        .filter(|&&i| nodes[i].kind == NodeKind::Commitment)
        .find_map(|&i| extract_file(&nodes[i]));

    let verify_cmd = seq
        .iter()
        .filter(|&&i| nodes[i].kind == NodeKind::Verification)
        .find_map(|&i| extract_command(&nodes[i]));

    let file_display = committed_file
        .as_deref()
        .map(short_path)
        .unwrap_or_else(|| "file".to_string());

    let cmd_display = verify_cmd
        .as_deref()
        .map(|c| truncate(c, 40))
        .unwrap_or_else(|| "tests".to_string());

    let summary = format!("Edited {} → verified with {}", file_display, cmd_display);

    let consolidated_ids: Vec<String> = seq.iter().map(|&i| nodes[i].id.clone()).collect();

    let detail = serde_json::json!({
        "pattern": "commit_and_verify",
        "file": committed_file,
        "verify_command": verify_cmd,
    });

    let id = next_id(counter, prefix);
    let timestamp = seq.last().map(|&i| nodes[i].timestamp).unwrap_or_default();

    let mut node = GraphNode::new(&id, NodeKind::Decision, timestamp, summary).with_detail(detail);
    node.classified = true;
    node.confidence = Some(0.80);
    node.consolidated_from = consolidated_ids;

    let decision_edges = build_decision_edges(nodes, seq, &id);

    Some((node, decision_edges))
}

/// Detect informed commit: explore → commit (without verification).
fn detect_informed_commit(
    nodes: &[GraphNode],
    seq: &[usize],
    counter: &mut u64,
    prefix: &str,
) -> Option<(GraphNode, Vec<GraphEdge>)> {
    let kinds: Vec<NodeKind> = seq.iter().map(|&i| nodes[i].kind).collect();

    let has_exploration = kinds.contains(&NodeKind::Exploration);
    let has_commitment = kinds.contains(&NodeKind::Commitment);
    let has_verification = kinds.contains(&NodeKind::Verification);

    if !(has_exploration && has_commitment && !has_verification) {
        return None;
    }

    let exploration_count = kinds
        .iter()
        .filter(|k| **k == NodeKind::Exploration)
        .count();

    let committed_file = seq
        .iter()
        .filter(|&&i| nodes[i].kind == NodeKind::Commitment)
        .find_map(|&i| extract_file(&nodes[i]));

    let file_display = committed_file
        .as_deref()
        .map(short_path)
        .unwrap_or_else(|| "file".to_string());

    let summary = format!(
        "Investigated {} file{} → edited {}",
        exploration_count,
        if exploration_count == 1 { "" } else { "s" },
        file_display
    );

    let consolidated_ids: Vec<String> = seq.iter().map(|&i| nodes[i].id.clone()).collect();

    let detail = serde_json::json!({
        "pattern": "informed_commit",
        "explorations": exploration_count,
        "file": committed_file,
    });

    let id = next_id(counter, prefix);
    let timestamp = seq.last().map(|&i| nodes[i].timestamp).unwrap_or_default();

    let mut node = GraphNode::new(&id, NodeKind::Decision, timestamp, summary).with_detail(detail);
    node.classified = true;
    node.confidence = Some(0.75);
    node.consolidated_from = consolidated_ids;

    let decision_edges = build_decision_edges(nodes, seq, &id);

    Some((node, decision_edges))
}

/// Detect systematic exploration: N consecutive explorations (no commits).
fn detect_systematic_exploration(
    nodes: &[GraphNode],
    seq: &[usize],
    counter: &mut u64,
    prefix: &str,
) -> Option<(GraphNode, Vec<GraphEdge>)> {
    let exploration_count = seq
        .iter()
        .filter(|&&i| nodes[i].kind == NodeKind::Exploration)
        .count();

    // Need at least 3 explorations and they must be the majority of the sequence
    if exploration_count < 3 || exploration_count < seq.len() / 2 {
        return None;
    }

    // Gather explored paths to find the common directory
    let paths: Vec<String> = seq
        .iter()
        .filter(|&&i| nodes[i].kind == NodeKind::Exploration)
        .filter_map(|&i| extract_file(&nodes[i]))
        .collect();

    let common_dir = common_directory(&paths);
    let summary = if common_dir.is_empty() {
        format!("Explored {} files", exploration_count)
    } else {
        format!("Explored {} files in {}", exploration_count, common_dir)
    };

    let consolidated_ids: Vec<String> = seq.iter().map(|&i| nodes[i].id.clone()).collect();

    let detail = serde_json::json!({
        "pattern": "systematic_exploration",
        "file_count": exploration_count,
        "common_directory": if common_dir.is_empty() { None } else { Some(&common_dir) },
        "files": paths,
    });

    let id = next_id(counter, prefix);
    let timestamp = seq.last().map(|&i| nodes[i].timestamp).unwrap_or_default();

    let mut node = GraphNode::new(&id, NodeKind::Decision, timestamp, summary).with_detail(detail);
    node.classified = true;
    node.confidence = Some(0.70);
    node.consolidated_from = consolidated_ids;

    let decision_edges = build_decision_edges(nodes, seq, &id);

    Some((node, decision_edges))
}

// =============================================================================
// Helpers
// =============================================================================

/// Extract a file path from a node's detail or summary.
fn extract_file(node: &GraphNode) -> Option<String> {
    // Try detail first
    if let Some(ref detail) = node.detail {
        // For commitment nodes: detail.file
        if let Some(file) = detail.get("file").and_then(|v| v.as_str()) {
            if !file.is_empty() {
                return Some(file.to_string());
            }
        }
        // For exploration nodes: detail.target
        if let Some(target) = detail.get("target").and_then(|v| v.as_str()) {
            if !target.is_empty() {
                return Some(target.to_string());
            }
        }
    }
    None
}

/// Extract a command string from a verification node's detail.
fn extract_command(node: &GraphNode) -> Option<String> {
    node.detail
        .as_ref()
        .and_then(|d| d.get("command"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Find the longest common directory prefix among a set of paths.
fn common_directory(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    if paths.len() == 1 {
        // Return the parent directory of the single file
        return paths[0]
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default();
    }

    // Split all paths into components and find the common prefix
    let components: Vec<Vec<&str>> = paths.iter().map(|p| p.split('/').collect()).collect();

    let mut common = Vec::new();
    let min_len = components.iter().map(|c| c.len()).min().unwrap_or(0);

    for i in 0..min_len {
        let first = components[0][i];
        if components.iter().all(|c| c[i] == first) {
            common.push(first);
        } else {
            break;
        }
    }

    // Don't include the filename component — only directories
    // If common has components and the last one matches a filename, pop it
    if !common.is_empty() {
        // If the common prefix equals one of the full paths, it includes
        // the filename — pop the last component
        let joined = common.join("/");
        if paths.iter().any(|p| p == &joined) {
            common.pop();
        }
    }

    common.join("/")
}

/// Build edges connecting a decision node to the surrounding context.
///
/// The decision node gets:
/// - An edge from the most recent goal (if any precedes the sequence)
/// - An edge to the next structural node after the sequence (if any)
fn build_decision_edges(nodes: &[GraphNode], seq: &[usize], decision_id: &str) -> Vec<GraphEdge> {
    let mut edges = Vec::new();

    let first_idx = match seq.first() {
        Some(&i) => i,
        None => return edges,
    };

    // Find the most recent goal before this sequence
    for i in (0..first_idx).rev() {
        if nodes[i].kind == NodeKind::Goal {
            edges.push(GraphEdge::new(
                nodes[i].id.clone(),
                decision_id.to_string(),
                EdgeKind::LedTo,
            ));
            break;
        }
    }

    // Find the next patch proposal after this sequence to link to
    let last_idx = seq.last().copied().unwrap_or(first_idx);
    for i in (last_idx + 1)..nodes.len() {
        if nodes[i].kind == NodeKind::PatchProposal {
            edges.push(GraphEdge::new(
                decision_id.to_string(),
                nodes[i].id.clone(),
                EdgeKind::CommittedVia,
            ));
            break;
        }
    }

    edges
}

/// Generate the next node ID.
fn next_id(counter: &mut u64, prefix: &str) -> String {
    *counter += 1;
    format!("{}-{}", prefix, counter)
}

/// Shorten a file path for summary display.
fn short_path(path: &str) -> String {
    // If the path has more than 3 components, show last 2
    let components: Vec<&str> = path.split('/').collect();
    if components.len() > 3 {
        components[components.len() - 2..].join("/")
    } else {
        path.to_string()
    }
}

/// Truncate a string for display.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Test helpers ----

    fn exploration(id: &str, file: &str) -> GraphNode {
        GraphNode::new(id, NodeKind::Exploration, 1000, format!("Read {}", file))
            .with_tool_name("read")
            .with_detail(serde_json::json!({"target": file}))
    }

    fn commitment(id: &str, file: &str) -> GraphNode {
        GraphNode::new(id, NodeKind::Commitment, 2000, format!("Edit {}", file))
            .with_tool_name("edit")
            .with_detail(serde_json::json!({"file": file, "tool": "edit"}))
    }

    fn verification(id: &str, cmd: &str, passed: bool) -> GraphNode {
        GraphNode::new(
            id,
            NodeKind::Verification,
            3000,
            format!("{} ({})", cmd, if passed { "passed" } else { "failed" }),
        )
        .with_tool_name("bash")
        .with_detail(serde_json::json!({"command": cmd, "passed": passed}))
    }

    fn goal(id: &str, summary: &str) -> GraphNode {
        GraphNode::new(id, NodeKind::Goal, 500, summary)
    }

    fn patch(id: &str) -> GraphNode {
        GraphNode::new(id, NodeKind::PatchProposal, 4000, "Change ABCD")
    }

    fn execution(id: &str, summary: &str) -> GraphNode {
        GraphNode::new(id, NodeKind::Execution, 1500, summary).with_tool_name("bash")
    }

    fn run_consolidate(
        nodes: &mut Vec<GraphNode>,
        edges: &mut Vec<GraphEdge>,
        stats: &mut GraphStats,
    ) -> u32 {
        let mut counter = nodes.len() as u64;
        consolidate(nodes, edges, stats, &mut counter, "test")
    }

    // ---- find_sequences ----

    #[test]
    fn test_find_sequences_simple() {
        let nodes = vec![
            goal("g1", "Fix bug"),
            exploration("e1", "src/a.rs"),
            exploration("e2", "src/b.rs"),
            commitment("c1", "src/a.rs"),
        ];
        let seqs = find_sequences(&nodes);
        assert_eq!(seqs.len(), 1);
        assert_eq!(seqs[0], vec![1, 2, 3]);
    }

    #[test]
    fn test_find_sequences_split_by_goal() {
        let nodes = vec![
            goal("g1", "First"),
            exploration("e1", "a.rs"),
            exploration("e2", "b.rs"),
            goal("g2", "Second"),
            exploration("e3", "c.rs"),
            commitment("c1", "c.rs"),
        ];
        let seqs = find_sequences(&nodes);
        assert_eq!(seqs.len(), 2);
        assert_eq!(seqs[0], vec![1, 2]);
        assert_eq!(seqs[1], vec![4, 5]);
    }

    #[test]
    fn test_find_sequences_single_node_dropped() {
        let nodes = vec![goal("g1", "Fix"), exploration("e1", "a.rs")];
        let seqs = find_sequences(&nodes);
        assert!(seqs.is_empty(), "single-node sequences should be dropped");
    }

    #[test]
    fn test_find_sequences_skips_classified() {
        let mut classified = exploration("e1", "a.rs");
        classified.classified = true;

        let nodes = vec![
            goal("g1", "Fix"),
            classified,
            exploration("e2", "b.rs"),
            commitment("c1", "b.rs"),
        ];
        let seqs = find_sequences(&nodes);
        assert_eq!(seqs.len(), 1);
        assert_eq!(seqs[0], vec![2, 3]);
    }

    // ---- Systematic exploration ----

    #[test]
    fn test_systematic_exploration() {
        let mut nodes = vec![
            goal("g1", "Understand the codebase"),
            exploration("e1", "src/auth/login.rs"),
            exploration("e2", "src/auth/jwt.rs"),
            exploration("e3", "src/auth/middleware.rs"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 3;

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        assert_eq!(created, 1);
        assert_eq!(stats.decision_count, 1);

        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        assert!(decision.classified);
        assert!(decision.summary.contains("3 files"));
        assert!(decision.summary.contains("src/auth"));
        assert_eq!(decision.consolidated_from.len(), 3); // IDs of the 3 exploration nodes
        assert!(decision.confidence.unwrap() > 0.0);

        // Original nodes should be marked as classified
        assert!(nodes[1].classified);
        assert!(nodes[2].classified);
        assert!(nodes[3].classified);
    }

    #[test]
    fn test_systematic_exploration_needs_3_minimum() {
        let mut nodes = vec![
            goal("g1", "Look around"),
            exploration("e1", "a.rs"),
            exploration("e2", "b.rs"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        // 2 explorations is below the threshold of 3
        assert_eq!(created, 0);
    }

    // ---- Informed commit ----

    #[test]
    fn test_informed_commit() {
        let mut nodes = vec![
            goal("g1", "Fix the bug"),
            exploration("e1", "src/auth.rs"),
            exploration("e2", "src/jwt.rs"),
            commitment("c1", "src/auth.rs"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 2;
        stats.commitment_count = 1;

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        assert_eq!(created, 1);

        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        assert!(decision.summary.contains("2 files"));
        assert!(decision.summary.contains("auth.rs"));

        let detail = decision.detail.as_ref().unwrap();
        assert_eq!(
            detail.get("pattern").unwrap().as_str().unwrap(),
            "informed_commit"
        );
    }

    // ---- Commit and verify ----

    #[test]
    fn test_commit_and_verify() {
        let mut nodes = vec![
            goal("g1", "Fix"),
            commitment("c1", "src/auth.rs"),
            verification("v1", "cargo test", true),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.commitment_count = 1;
        stats.verification_count = 1;

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        assert_eq!(created, 1);

        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        assert!(decision.summary.contains("auth.rs"));
        assert!(decision.summary.contains("cargo test"));

        let detail = decision.detail.as_ref().unwrap();
        assert_eq!(
            detail.get("pattern").unwrap().as_str().unwrap(),
            "commit_and_verify"
        );
    }

    // ---- Full cycle ----

    #[test]
    fn test_full_cycle() {
        let mut nodes = vec![
            goal("g1", "Fix the auth bug"),
            exploration("e1", "src/auth/login.rs"),
            exploration("e2", "src/auth/jwt.rs"),
            commitment("c1", "src/auth/login.rs"),
            verification("v1", "cargo test --lib", true),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 2;
        stats.commitment_count = 1;
        stats.verification_count = 1;

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        assert_eq!(created, 1);

        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        assert!(decision.summary.contains("2 files"));
        assert!(decision.summary.contains("login.rs"));
        assert!(decision.summary.contains("cargo test"));

        let detail = decision.detail.as_ref().unwrap();
        assert_eq!(
            detail.get("pattern").unwrap().as_str().unwrap(),
            "full_cycle"
        );
    }

    // ---- Backtracking ----

    #[test]
    fn test_backtracking_detected() {
        let mut nodes = vec![
            goal("g1", "Fix"),
            exploration("e1", "src/auth.rs"),
            commitment("c1", "src/auth.rs"),
            exploration("e2", "src/auth.rs"),
            commitment("c2", "src/auth.rs"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 2;
        stats.commitment_count = 2;

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        assert_eq!(created, 1);

        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        assert!(decision.summary.contains("Iterated"));
        assert!(decision.summary.contains("2 attempts"));
        assert!(decision.summary.contains("auth.rs"));

        let detail = decision.detail.as_ref().unwrap();
        assert_eq!(
            detail.get("pattern").unwrap().as_str().unwrap(),
            "backtracking"
        );
        assert_eq!(detail.get("iterations").unwrap().as_u64().unwrap(), 2);
    }

    #[test]
    fn test_backtracking_not_detected_single_edit() {
        let mut nodes = vec![
            goal("g1", "Fix"),
            exploration("e1", "src/auth.rs"),
            commitment("c1", "src/auth.rs"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 1;
        stats.commitment_count = 1;

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        // Should match informed_commit, not backtracking
        if created > 0 {
            let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
            let detail = decision.detail.as_ref().unwrap();
            assert_ne!(
                detail.get("pattern").unwrap().as_str().unwrap(),
                "backtracking"
            );
        }
    }

    // ---- Test-driven iteration ----

    #[test]
    fn test_test_driven_iteration() {
        let mut nodes = vec![
            goal("g1", "Fix test failure"),
            commitment("c1", "src/auth.rs"),
            verification("v1", "cargo test", false),
            commitment("c2", "src/jwt.rs"),
            verification("v2", "cargo test", true),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.commitment_count = 2;
        stats.verification_count = 2;

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        assert_eq!(created, 1);

        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        assert!(decision.summary.contains("Test-driven"));
        assert!(decision.summary.contains("auth.rs") || decision.summary.contains("jwt.rs"));

        let detail = decision.detail.as_ref().unwrap();
        assert_eq!(
            detail.get("pattern").unwrap().as_str().unwrap(),
            "test_driven_iteration"
        );
    }

    #[test]
    fn test_test_driven_not_detected_without_fail() {
        let mut nodes = vec![
            goal("g1", "Add feature"),
            commitment("c1", "src/auth.rs"),
            verification("v1", "cargo test", true),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        // Should match commit_and_verify, not test_driven_iteration
        if created > 0 {
            let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
            let detail = decision.detail.as_ref().unwrap();
            assert_ne!(
                detail.get("pattern").unwrap().as_str().unwrap(),
                "test_driven_iteration"
            );
        }
    }

    // ---- Idempotency ----

    #[test]
    fn test_consolidation_is_idempotent() {
        let mut nodes = vec![
            goal("g1", "Fix"),
            exploration("e1", "src/auth.rs"),
            exploration("e2", "src/jwt.rs"),
            commitment("c1", "src/auth.rs"),
            verification("v1", "cargo test", true),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 2;
        stats.commitment_count = 1;
        stats.verification_count = 1;

        let created1 = run_consolidate(&mut nodes, &mut edges, &mut stats);
        assert_eq!(created1, 1);

        let node_count_after_first = nodes.len();
        let edge_count_after_first = edges.len();

        // Run again — should create nothing new
        let created2 = run_consolidate(&mut nodes, &mut edges, &mut stats);
        assert_eq!(created2, 0, "second consolidation should be a no-op");
        assert_eq!(nodes.len(), node_count_after_first);
        assert_eq!(edges.len(), edge_count_after_first);
    }

    // ---- Multiple sequences ----

    #[test]
    fn test_multiple_sequences_in_one_turn() {
        let mut nodes = vec![
            goal("g1", "First task"),
            exploration("e1", "src/a.rs"),
            exploration("e2", "src/b.rs"),
            commitment("c1", "src/a.rs"),
            // goal boundary
            goal("g2", "Second task"),
            exploration("e3", "src/c.rs"),
            exploration("e4", "src/d.rs"),
            exploration("e5", "src/e.rs"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 2;
        stats.exploration_count = 5;
        stats.commitment_count = 1;

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        // Should create 2 decisions: informed_commit for first, systematic_exploration for second
        assert_eq!(created, 2);
        assert_eq!(stats.decision_count, 2);
    }

    // ---- Edge cases ----

    #[test]
    fn test_empty_graph() {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);
        assert_eq!(created, 0);
    }

    #[test]
    fn test_only_goals() {
        let mut nodes = vec![goal("g1", "First"), goal("g2", "Second")];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();

        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);
        assert_eq!(created, 0);
    }

    #[test]
    fn test_mixed_with_execution() {
        let mut nodes = vec![
            goal("g1", "Setup"),
            execution("x1", "npm install"),
            exploration("e1", "src/a.rs"),
            commitment("c1", "src/a.rs"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.execution_count = 1;
        stats.exploration_count = 1;
        stats.commitment_count = 1;

        // execution + exploration + commitment = 3 nodes, should form a sequence
        let created = run_consolidate(&mut nodes, &mut edges, &mut stats);

        // Might match informed_commit or full_cycle depending on execution handling
        assert!(created <= 1);
    }

    #[test]
    fn test_decision_node_gets_goal_edge() {
        let mut nodes = vec![
            goal("g1", "Fix bug"),
            exploration("e1", "src/a.rs"),
            exploration("e2", "src/b.rs"),
            exploration("e3", "src/c.rs"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 3;

        run_consolidate(&mut nodes, &mut edges, &mut stats);

        // The decision should have an edge from the goal
        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.from == "g1" && e.to == decision.id && e.kind == EdgeKind::LedTo),
            "decision should have led_to edge from goal"
        );
    }

    #[test]
    fn test_decision_node_links_to_patch() {
        let mut nodes = vec![
            goal("g1", "Fix"),
            exploration("e1", "src/a.rs"),
            commitment("c1", "src/a.rs"),
            verification("v1", "cargo test", true),
            patch("p1"),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 1;
        stats.commitment_count = 1;
        stats.verification_count = 1;
        stats.patch_proposal_count = 1;

        run_consolidate(&mut nodes, &mut edges, &mut stats);

        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.from == decision.id && e.to == "p1" && e.kind == EdgeKind::CommittedVia),
            "decision should have committed_via edge to patch"
        );
    }

    // ---- Priority: backtracking wins over full_cycle ----

    #[test]
    fn test_backtracking_takes_priority() {
        let mut nodes = vec![
            goal("g1", "Fix"),
            exploration("e1", "src/auth.rs"),
            commitment("c1", "src/auth.rs"),
            exploration("e2", "src/auth.rs"),
            commitment("c2", "src/auth.rs"),
            verification("v1", "cargo test", true),
        ];
        let mut edges = Vec::new();
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 2;
        stats.commitment_count = 2;
        stats.verification_count = 1;

        run_consolidate(&mut nodes, &mut edges, &mut stats);

        let decision = nodes.iter().find(|n| n.kind == NodeKind::Decision).unwrap();
        let detail = decision.detail.as_ref().unwrap();
        // Backtracking should win because auth.rs was edited twice
        assert_eq!(
            detail.get("pattern").unwrap().as_str().unwrap(),
            "backtracking"
        );
    }

    // ---- common_directory ----

    #[test]
    fn test_common_directory_same_dir() {
        let paths = vec![
            "src/auth/login.rs".to_string(),
            "src/auth/jwt.rs".to_string(),
            "src/auth/middleware.rs".to_string(),
        ];
        assert_eq!(common_directory(&paths), "src/auth");
    }

    #[test]
    fn test_common_directory_different_dirs() {
        let paths = vec![
            "src/auth/login.rs".to_string(),
            "src/db/connection.rs".to_string(),
        ];
        assert_eq!(common_directory(&paths), "src");
    }

    #[test]
    fn test_common_directory_no_common() {
        let paths = vec!["auth.rs".to_string(), "db.rs".to_string()];
        assert_eq!(common_directory(&paths), "");
    }

    #[test]
    fn test_common_directory_single_file() {
        let paths = vec!["src/auth/login.rs".to_string()];
        assert_eq!(common_directory(&paths), "src/auth");
    }

    #[test]
    fn test_common_directory_empty() {
        let paths: Vec<String> = Vec::new();
        assert_eq!(common_directory(&paths), "");
    }

    // ---- short_path ----

    #[test]
    fn test_short_path_long() {
        assert_eq!(
            short_path("src/auth/middleware/validation.rs"),
            "middleware/validation.rs"
        );
    }

    #[test]
    fn test_short_path_short() {
        assert_eq!(short_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_short_path_no_directory() {
        assert_eq!(short_path("main.rs"), "main.rs");
    }
}
