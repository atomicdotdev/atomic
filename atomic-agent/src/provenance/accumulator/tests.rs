use super::helpers::{make_session_prefix, short_hash, truncate_prompt};
use super::*;
use crate::provenance::types::{EdgeKind, NodeKind, SerializedGraph};

// ---- Construction ----

#[test]
fn test_new_accumulator_is_empty() {
    let acc = ProvenanceAccumulator::new("test-session");
    assert_eq!(acc.session_id(), "test-session");
    assert!(acc.is_empty());
    assert_eq!(acc.node_count(), 0);
    assert_eq!(acc.edge_count(), 0);
    assert!(acc.stats().is_empty());
    assert!(acc.current_goal().is_none());
}

// ---- append_goal ----

#[test]
fn test_append_goal() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let id = acc.append_goal("Fix the auth bug", 1000);

    assert!(!id.is_empty());
    assert_eq!(acc.node_count(), 1);
    assert_eq!(acc.stats().goal_count, 1);
    assert_eq!(acc.current_goal(), Some(id.as_str()));

    let node = &acc.nodes()[0];
    assert_eq!(node.kind, NodeKind::Goal);
    assert_eq!(node.summary, "Fix the auth bug");
    assert_eq!(node.timestamp, 1000);
}

#[test]
fn test_chained_goals() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let g1 = acc.append_goal("First goal", 1000);
    let g2 = acc.append_goal("Second goal", 2000);

    assert_eq!(acc.stats().goal_count, 2);
    assert_eq!(acc.current_goal(), Some(g2.as_str()));

    // Should have a led_to edge from g1 → g2
    assert_eq!(acc.edge_count(), 1);
    let edge = &acc.edges()[0];
    assert_eq!(edge.from, g1);
    assert_eq!(edge.to, g2);
    assert_eq!(edge.kind, EdgeKind::LedTo);
}

#[test]
fn test_goal_resets_pending_explorations() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let _g1 = acc.append_goal("First", 1000);
    let _r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let _r2 = acc.append_tool_call("read", Some("c2"), None, None, None, None, 1002);

    // pending_explorations should have 2 entries
    assert_eq!(acc.pending_explorations.len(), 2);

    // New goal resets them
    let _g2 = acc.append_goal("Second", 2000);
    assert_eq!(acc.pending_explorations.len(), 0);
}

// ---- append_tool_call: exploration ----

#[test]
fn test_append_exploration() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let goal = acc.append_goal("Fix bug", 1000);
    let read_id = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);

    assert_eq!(acc.node_count(), 2);
    assert_eq!(acc.stats().exploration_count, 1);

    // Should have edge: goal --led_to-→ read
    let edge = acc
        .edges()
        .iter()
        .find(|e| e.from == goal && e.to == read_id)
        .expect("should have goal → exploration edge");
    assert_eq!(edge.kind, EdgeKind::LedTo);
}

#[test]
fn test_multiple_explorations_all_link_to_goal() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let goal = acc.append_goal("Fix bug", 1000);
    let r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let r2 = acc.append_tool_call("grep", Some("c2"), None, None, None, None, 1002);
    let r3 = acc.append_tool_call("list_directory", Some("c3"), None, None, None, None, 1003);

    assert_eq!(acc.stats().exploration_count, 3);

    // All explorations should have led_to edges from goal
    for rid in [&r1, &r2, &r3] {
        assert!(
            acc.edges()
                .iter()
                .any(|e| e.from == goal && e.to == *rid && e.kind == EdgeKind::LedTo),
            "goal should lead to exploration {}",
            rid
        );
    }
}

// ---- append_tool_call: commitment ----

#[test]
fn test_commitment_with_preceding_explorations() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let _goal = acc.append_goal("Fix bug", 1000);
    let r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let r2 = acc.append_tool_call("read", Some("c2"), None, None, None, None, 1002);
    let edit = acc.append_tool_call("edit", Some("c3"), None, None, None, None, 1003);

    assert_eq!(acc.stats().commitment_count, 1);

    // Explorations → commitment via explored_via
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == r1 && e.to == edit && e.kind == EdgeKind::ExploredVia));
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == r2 && e.to == edit && e.kind == EdgeKind::ExploredVia));

    // Pending explorations should be cleared
    assert!(acc.pending_explorations.is_empty());
}

#[test]
fn test_commitment_without_explorations_links_to_goal() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let goal = acc.append_goal("Fix bug", 1000);
    let edit = acc.append_tool_call("edit", Some("c1"), None, None, None, None, 1001);

    // Should link directly from goal
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == goal && e.to == edit && e.kind == EdgeKind::LedTo));
}

// ---- append_tool_call: verification ----

#[test]
fn test_verification_links_to_commitment() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let _goal = acc.append_goal("Fix bug", 1000);
    let edit = acc.append_tool_call("edit", Some("c1"), None, None, None, None, 1001);

    let test_input = serde_json::json!({"command": "cargo test"});
    let test = acc.append_tool_call(
        "bash",
        Some("c2"),
        Some(&test_input),
        None,
        None,
        None,
        1002,
    );

    assert_eq!(acc.stats().verification_count, 1);

    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == edit && e.to == test && e.kind == EdgeKind::VerifiedBy));
}

#[test]
fn test_verification_without_commitment_links_to_goal() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let goal = acc.append_goal("Run tests", 1000);

    let test_input = serde_json::json!({"command": "cargo test"});
    let test = acc.append_tool_call(
        "bash",
        Some("c1"),
        Some(&test_input),
        None,
        None,
        None,
        1001,
    );

    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == goal && e.to == test && e.kind == EdgeKind::LedTo));
}

#[test]
fn test_node_tap_zero_failures_is_passed_in_summary_and_detail() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let input = serde_json::json!({"command": "npm test"});
    let output = "TAP version 13\nℹ tests 3\nℹ pass 3\nℹ fail 0";

    let node_id = acc.append_tool_call(
        "bash",
        Some("tap-1"),
        Some(&input),
        Some(output),
        None,
        None,
        1000,
    );
    let node = acc.nodes().iter().find(|node| node.id == node_id).unwrap();

    assert_eq!(node.summary, "npm test (passed)");
    assert_eq!(node.detail.as_ref().unwrap()["passed"], true);
}

#[test]
fn test_completed_tool_status_does_not_hide_failed_verification() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let input = serde_json::json!({"command": "npm test"});

    let node_id = acc.append_tool_call(
        "bash",
        Some("tap-failed"),
        Some(&input),
        Some("test auth flow ... FAILED"),
        Some("completed"),
        None,
        1000,
    );
    let node = acc.nodes().iter().find(|node| node.id == node_id).unwrap();

    assert_eq!(node.summary, "npm test (failed)");
    assert_eq!(node.detail.as_ref().unwrap()["passed"], false);
}

// ---- append_tool_call: execution ----

#[test]
fn test_execution_links_to_goal() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let goal = acc.append_goal("Setup", 1000);

    let install_input = serde_json::json!({"command": "npm install express"});
    let install = acc.append_tool_call(
        "bash",
        Some("c1"),
        Some(&install_input),
        None,
        None,
        None,
        1001,
    );

    assert_eq!(acc.stats().execution_count, 1);
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == goal && e.to == install && e.kind == EdgeKind::LedTo));
}

// ---- append_tool_call: error ----

#[test]
fn test_error_links_to_last_node() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let _goal = acc.append_goal("Fix bug", 1000);
    let read = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let error = acc.append_tool_call("edit", Some("c2"), None, None, Some("error"), None, 1002);

    assert_eq!(acc.stats().error_count, 1);
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == read && e.to == error && e.kind == EdgeKind::FailedWith));
}

// ---- append_human_gate ----

#[test]
fn test_human_gate() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let _goal = acc.append_goal("Fix bug", 1000);
    let read = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let gate = acc.append_human_gate("Delete old tokens?", 1002);

    assert_eq!(acc.stats().human_gate_count, 1);
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == read && e.to == gate && e.kind == EdgeKind::BlockedBy));
    assert_eq!(acc.pending_human_gate.as_deref(), Some(gate.as_str()));
}

#[test]
fn test_resolve_human_gate() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let gate = acc.append_human_gate("Delete old tokens?", 1000);

    acc.resolve_human_gate(&gate);

    let node = acc.nodes().iter().find(|n| n.id == gate).unwrap();
    let resolved = node
        .detail
        .as_ref()
        .unwrap()
        .get("resolved")
        .unwrap()
        .as_bool()
        .unwrap();
    assert!(resolved);
    assert!(acc.pending_human_gate.is_none());
}

#[test]
fn test_goal_after_gate_has_resumed_edge() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let gate = acc.append_human_gate("Should I proceed?", 1000);
    let goal = acc.append_goal("Yes, proceed", 1001);

    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == gate && e.to == goal && e.kind == EdgeKind::ResumedAfter));
}

// ---- append_patch_proposal ----

#[test]
fn test_patch_proposal_links_to_commitments() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let _goal = acc.append_goal("Fix bug", 1000);
    let edit1 = acc.append_tool_call("edit", Some("c1"), None, None, None, None, 1001);
    let edit2 = acc.append_tool_call("write", Some("c2"), None, None, None, None, 1002);
    let patch = acc.append_patch_proposal(
        "ABCD1234EFGH5678",
        &["src/a.rs".into(), "src/b.rs".into()],
        1003,
    );

    assert_eq!(acc.stats().patch_proposal_count, 1);

    // Both commits → patch via committed_via
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == edit1 && e.to == patch && e.kind == EdgeKind::CommittedVia));
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == edit2 && e.to == patch && e.kind == EdgeKind::CommittedVia));

    // commitments_since_last_patch should be cleared
    assert!(acc.commitments_since_last_patch.is_empty());
}

#[test]
fn test_patch_proposal_without_commitments_links_to_goal() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let goal = acc.append_goal("Fix bug", 1000);
    let patch = acc.append_patch_proposal("ABCD", &[], 1001);

    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == goal && e.to == patch && e.kind == EdgeKind::LedTo));
}

#[test]
fn test_patch_proposal_display_single_file() {
    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_patch_proposal("ABCDEF123456", &["src/main.rs".into()], 1000);

    let node = &acc.nodes()[0];
    assert!(node.summary.contains("ABCDEF12"));
    assert!(node.summary.contains("src/main.rs"));
}

#[test]
fn test_patch_proposal_display_multiple_files() {
    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_patch_proposal(
        "HASH1234",
        &["a.rs".into(), "b.rs".into(), "c.rs".into()],
        1000,
    );

    let node = &acc.nodes()[0];
    assert!(node.summary.contains("3 files"));
}

// ---- Full session flow ----

#[test]
fn test_typical_session_graph_structure() {
    let mut acc = ProvenanceAccumulator::new("test-session");

    // Human asks to fix a bug
    let goal = acc.append_goal("Fix the auth bug", 1000);

    // Agent reads 3 files
    let r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let r2 = acc.append_tool_call("read", Some("c2"), None, None, None, None, 1002);
    let r3 = acc.append_tool_call("read", Some("c3"), None, None, None, None, 1003);

    // Agent edits one file
    let edit = acc.append_tool_call("edit", Some("c4"), None, None, None, None, 1004);

    // Agent runs tests
    let test_input = serde_json::json!({"command": "cargo test"});
    let test = acc.append_tool_call(
        "bash",
        Some("c5"),
        Some(&test_input),
        None,
        None,
        None,
        1005,
    );

    // Verify node counts
    assert_eq!(acc.node_count(), 6);
    assert_eq!(acc.stats().goal_count, 1);
    assert_eq!(acc.stats().exploration_count, 3);
    assert_eq!(acc.stats().commitment_count, 1);
    assert_eq!(acc.stats().verification_count, 1);

    // Verify edges
    // goal → r1, r2, r3 (led_to)
    for r in [&r1, &r2, &r3] {
        assert!(
            acc.edges()
                .iter()
                .any(|e| e.from == goal && e.to == *r && e.kind == EdgeKind::LedTo),
            "goal → {} led_to",
            r
        );
    }

    // r1, r2, r3 → edit (explored_via)
    for r in [&r1, &r2, &r3] {
        assert!(
            acc.edges()
                .iter()
                .any(|e| e.from == *r && e.to == edit && e.kind == EdgeKind::ExploredVia),
            "{} → edit explored_via",
            r
        );
    }

    // edit → test (verified_by)
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == edit && e.to == test && e.kind == EdgeKind::VerifiedBy));
}

#[test]
fn test_multi_turn_session() {
    let mut acc = ProvenanceAccumulator::new("s1");

    // Turn 1: fix a bug
    let g1 = acc.append_goal("Fix auth bug", 1000);
    let _r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let e1 = acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);
    let _p1 = acc.append_patch_proposal("HASH1", &["auth.rs".into()], 1003);

    // Turn 2: add tests
    let g2 = acc.append_goal("Add tests", 2000);
    let _r2 = acc.append_tool_call("read", Some("c3"), None, None, None, None, 2001);
    let e2 = acc.append_tool_call("write", Some("c4"), None, None, None, None, 2002);

    let test_input = serde_json::json!({"command": "cargo test"});
    let _t2 = acc.append_tool_call(
        "bash",
        Some("c5"),
        Some(&test_input),
        None,
        None,
        None,
        2003,
    );
    let _p2 = acc.append_patch_proposal("HASH2", &["test_auth.rs".into()], 2004);

    assert_eq!(acc.stats().goal_count, 2);
    assert_eq!(acc.stats().patch_proposal_count, 2);

    // g1 → g2 chained
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == g1 && e.to == g2 && e.kind == EdgeKind::LedTo));

    // First patch only linked to first edit
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == e1 && e.kind == EdgeKind::CommittedVia));
    // Second patch linked to second edit, not first
    assert!(acc
        .edges()
        .iter()
        .any(|e| e.from == e2 && e.kind == EdgeKind::CommittedVia));
}

// ---- Serialization round-trip ----

#[test]
fn test_serialized_graph_roundtrip() {
    let mut acc = ProvenanceAccumulator::new("roundtrip-test");
    acc.append_goal("Fix bug", 1000);
    acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

    let serialized = acc.to_serialized_graph();
    let json = serde_json::to_string_pretty(&serialized).unwrap();
    let deserialized: SerializedGraph = serde_json::from_str(&json).unwrap();

    let restored = ProvenanceAccumulator::from_serialized(deserialized);

    assert_eq!(restored.node_count(), acc.node_count());
    assert_eq!(restored.edge_count(), acc.edge_count());
    assert_eq!(restored.session_id(), acc.session_id());
    assert_eq!(restored.current_goal(), acc.current_goal());
    assert_eq!(restored.counter, acc.counter);
}

#[test]
fn test_serialization_preserves_accumulator_state() {
    let mut acc = ProvenanceAccumulator::new("state-test");
    acc.append_goal("Goal", 1000);
    let _r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

    let serialized = acc.to_serialized_graph();

    // Accumulator state should be preserved
    assert!(serialized.current_goal.is_some());
    // After the edit, pending explorations should be cleared
    assert!(serialized.pending_explorations.is_empty());
    assert!(serialized.last_commitment.is_some());
    assert!(serialized.last_node.is_some());
}

#[test]
fn test_from_serialized_rebuilds_commitments_since_patch() {
    let mut acc = ProvenanceAccumulator::new("rebuild-test");
    acc.append_goal("Fix", 1000);
    let e1 = acc.append_tool_call("edit", Some("c1"), None, None, None, None, 1001);
    acc.append_patch_proposal("HASH1", &[], 1002);
    let e2 = acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1003);
    let e3 = acc.append_tool_call("write", Some("c3"), None, None, None, None, 1004);

    let serialized = acc.to_serialized_graph();
    let restored = ProvenanceAccumulator::from_serialized(serialized);

    // Should only have e2 and e3, not e1 (which was before the patch)
    assert_eq!(restored.commitments_since_last_patch.len(), 2);
    assert!(restored.commitments_since_last_patch.contains(&e2));
    assert!(restored.commitments_since_last_patch.contains(&e3));
    assert!(!restored.commitments_since_last_patch.contains(&e1));
}

// ---- to_provenance_graph conversion ----

#[test]
fn test_to_provenance_graph_basic() {
    use atomic_core::change::provenance_graph as pg;
    use atomic_core::types::Hash;

    let mut acc = ProvenanceAccumulator::new("sess-convert");
    acc.append_goal("Fix the auth bug", 1000);
    acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

    let change_hash = Hash::of(b"test-change");
    let graph = acc.to_provenance_graph("claude-code", "Claude Code", "anthropic", &[change_hash]);

    assert_eq!(graph.session_id, "sess-convert");
    assert_eq!(graph.agent_name, "claude-code");
    assert_eq!(graph.agent_display_name, "Claude Code");
    assert_eq!(graph.agent_vendor, "anthropic");
    assert_eq!(graph.nodes.len(), 3);
    assert_eq!(graph.edges.len(), acc.edge_count());
    assert_eq!(graph.changes_explained, vec![change_hash]);
    assert!(graph.previous.is_none());

    // Verify node kinds were converted correctly
    assert_eq!(graph.nodes[0].kind, pg::ProvenanceNodeKind::Goal);
    assert_eq!(graph.nodes[1].kind, pg::ProvenanceNodeKind::Exploration);
    assert_eq!(graph.nodes[2].kind, pg::ProvenanceNodeKind::Commitment);

    // Verify stats were auto-computed
    assert_eq!(graph.stats.goal_count, 1);
    assert_eq!(graph.stats.exploration_count, 1);
    assert_eq!(graph.stats.commitment_count, 1);
}

#[test]
fn test_to_provenance_graph_with_chaining() {
    use atomic_core::types::{Base32, Hash};

    let mut acc = ProvenanceAccumulator::new("sess-chain");
    acc.append_goal("Second turn", 2000);

    let prev_hash = Hash::of(b"previous-graph");
    acc.set_last_provenance_hash(prev_hash.to_base32());
    let graph = acc.to_provenance_graph("opencode", "OpenCode", "anthropic", &[]);

    assert!(graph.is_chained());
    assert_eq!(graph.previous, Some(prev_hash));
}

#[test]
fn test_to_provenance_graph_serializes_cleanly() {
    use atomic_core::change::provenance_graph as pg;

    let mut acc = ProvenanceAccumulator::new("sess-serialize");
    acc.append_goal("Fix bug", 1000);

    let input = serde_json::json!({"path": "src/auth.rs"});
    acc.append_tool_call("read", Some("c1"), Some(&input), None, None, None, 1001);
    acc.append_tool_call(
        "edit",
        Some("c2"),
        Some(&input),
        None,
        None,
        Some(150),
        1002,
    );

    let test_input = serde_json::json!({"command": "cargo test"});
    acc.append_tool_call(
        "bash",
        Some("c3"),
        Some(&test_input),
        Some("test result: ok"),
        None,
        Some(3200),
        1003,
    );

    let graph = acc.to_provenance_graph("agent", "Agent", "vendor", &[]);

    // Should serialize and deserialize via postcard (content-addressed format)
    let bytes = graph.serialize().unwrap();
    let (loaded, _hash) = pg::ProvenanceGraph::deserialize(&bytes).unwrap();

    assert_eq!(loaded.nodes.len(), 4);
    assert_eq!(loaded.edges.len(), graph.edges.len());
    assert_eq!(loaded.session_id, "sess-serialize");

    // Tool metadata should survive the round-trip
    let read_node = &loaded.nodes[1];
    assert_eq!(read_node.tool_name.as_deref(), Some("read"));

    let test_node = &loaded.nodes[3];
    assert_eq!(test_node.duration_ms, Some(3200));
}

#[test]
fn test_to_provenance_graph_edge_kinds_convert() {
    use atomic_core::change::provenance_graph as pg;

    let mut acc = ProvenanceAccumulator::new("sess-edges");
    let _goal = acc.append_goal("Fix", 1000);
    let _read = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let _edit = acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

    let test_input = serde_json::json!({"command": "cargo test"});
    let _test = acc.append_tool_call(
        "bash",
        Some("c3"),
        Some(&test_input),
        None,
        None,
        None,
        1003,
    );

    let graph = acc.to_provenance_graph("a", "A", "v", &[]);

    // Verify edge kinds converted
    let edge_kinds: Vec<pg::ProvenanceEdgeKind> = graph.edges.iter().map(|e| e.kind).collect();

    assert!(edge_kinds.contains(&pg::ProvenanceEdgeKind::LedTo));
    assert!(edge_kinds.contains(&pg::ProvenanceEdgeKind::ExploredVia));
    assert!(edge_kinds.contains(&pg::ProvenanceEdgeKind::VerifiedBy));
}

// ---- Compaction summary ----

#[test]
fn test_compaction_summary_empty() {
    let acc = ProvenanceAccumulator::new("s1");
    let summary = acc.to_compaction_summary();
    assert!(summary.contains("0 nodes"));
}

#[test]
fn test_compaction_summary_has_goals() {
    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_goal("Fix the auth bug", 1000);

    let summary = acc.to_compaction_summary();
    assert!(summary.contains("### Goals"));
    assert!(summary.contains("Fix the auth bug"));
}

#[test]
fn test_compaction_summary_has_changes() {
    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_goal("Fix bug", 1000);

    let input = serde_json::json!({"path": "src/auth.rs"});
    acc.append_tool_call("edit", Some("c1"), Some(&input), None, None, None, 1001);

    let summary = acc.to_compaction_summary();
    assert!(summary.contains("### Changes Made"));
    assert!(summary.contains("Edit src/auth.rs"));
}

#[test]
fn test_compaction_summary_has_patches() {
    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_patch_proposal("ABCD1234", &["src/main.rs".into()], 1000);

    let summary = acc.to_compaction_summary();
    assert!(summary.contains("### Recorded Changes"));
}

#[test]
fn test_compaction_summary_has_human_gates() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let gate = acc.append_human_gate("Should I delete old tokens?", 1000);

    let summary = acc.to_compaction_summary();
    assert!(summary.contains("### Human Gates"));
    assert!(summary.contains("pending"));

    acc.resolve_human_gate(&gate);
    let summary = acc.to_compaction_summary();
    assert!(summary.contains("resolved"));
}

#[test]
fn test_compaction_summary_has_errors() {
    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_tool_call(
        "edit",
        Some("c1"),
        None,
        Some("File not found"),
        Some("error"),
        None,
        1000,
    );

    let summary = acc.to_compaction_summary();
    assert!(summary.contains("### Errors"));
    assert!(summary.contains("failed"));
}

// ---- Persistence ----

#[test]
fn test_save_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("test-session");

    let mut acc = ProvenanceAccumulator::new("test-session");
    acc.append_goal("Fix bug", 1000);
    acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

    acc.save(&session_dir).unwrap();

    // Verify file exists
    assert!(session_dir.join(GRAPH_FILENAME).exists());

    // Load it back
    let restored = ProvenanceAccumulator::load_or_create(&session_dir, "test-session").unwrap();
    assert_eq!(restored.node_count(), 3);
    assert_eq!(restored.edge_count(), acc.edge_count());
    assert_eq!(restored.session_id(), "test-session");
}

#[test]
fn test_load_nonexistent_creates_empty() {
    let dir = tempfile::tempdir().unwrap();
    let acc = ProvenanceAccumulator::load_or_create(dir.path(), "no-such-session").unwrap();
    assert!(acc.is_empty());
    assert_eq!(acc.session_id(), "no-such-session");
}

#[test]
fn test_save_creates_directory() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("deep").join("nested").join("session");

    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_goal("Test", 1000);
    acc.save(&nested).unwrap();

    assert!(nested.join(GRAPH_FILENAME).exists());
}

#[test]
fn test_incremental_save_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("s1");

    // First hook invocation: create session + goal
    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_goal("Fix bug", 1000);
    acc.save(&session_dir).unwrap();

    // Second hook invocation: load + append tool call
    let mut acc = ProvenanceAccumulator::load_or_create(&session_dir, "s1").unwrap();
    assert_eq!(acc.node_count(), 1);
    acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    acc.save(&session_dir).unwrap();

    // Third hook invocation: load + append another tool call
    let mut acc = ProvenanceAccumulator::load_or_create(&session_dir, "s1").unwrap();
    assert_eq!(acc.node_count(), 2);
    acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);
    acc.save(&session_dir).unwrap();

    // Final verification
    let acc = ProvenanceAccumulator::load_or_create(&session_dir, "s1").unwrap();
    assert_eq!(acc.node_count(), 3);
    assert_eq!(acc.stats().goal_count, 1);
    assert_eq!(acc.stats().exploration_count, 1);
    assert_eq!(acc.stats().commitment_count, 1);

    // Edges should be preserved through save/load cycles
    assert!(acc.edge_count() > 0);
}

// ---- Helper functions ----

#[test]
fn test_make_session_prefix_uuid() {
    let prefix = make_session_prefix("abc123de-f456-7890-abcd-ef1234567890");
    assert_eq!(prefix, "abc123de");
}

#[test]
fn test_make_session_prefix_short() {
    let prefix = make_session_prefix("ab");
    assert_eq!(prefix, "ab");
}

#[test]
fn test_make_session_prefix_empty() {
    let prefix = make_session_prefix("");
    assert_eq!(prefix, "s");
}

#[test]
fn test_truncate_prompt_short() {
    assert_eq!(truncate_prompt("hello", 100), "hello");
}

#[test]
fn test_truncate_prompt_long() {
    let long = "a ".repeat(300);
    let result = truncate_prompt(&long, 50);
    assert!(result.len() <= 50);
    assert!(result.ends_with("..."));
}

#[test]
fn test_truncate_prompt_trims() {
    assert_eq!(truncate_prompt("  hello  ", 100), "hello");
}

#[test]
fn test_short_hash() {
    assert_eq!(short_hash("ABCDEF1234567890"), "ABCDEF12");
    assert_eq!(short_hash("SHORT"), "SHORT");
}

// ---- Node ID uniqueness ----

#[test]
fn test_node_ids_are_unique() {
    let mut acc = ProvenanceAccumulator::new("s1");
    let ids = vec![
        acc.append_goal("g1", 1000),
        acc.append_tool_call("read", None, None, None, None, None, 1001),
        acc.append_tool_call("edit", None, None, None, None, None, 1002),
        acc.append_human_gate("gate", 1003),
        acc.append_patch_proposal("HASH", &[], 1004),
    ];

    // All IDs should be unique
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "all node IDs should be unique");
}

#[test]
fn test_node_ids_contain_session_prefix() {
    let mut acc = ProvenanceAccumulator::new("my-session-id");
    let id = acc.append_goal("test", 1000);
    assert!(
        id.starts_with("my"),
        "node ID should start with session prefix, got: {}",
        id
    );
}

// ---- Stats consistency ----

#[test]
fn test_stats_match_nodes() {
    let mut acc = ProvenanceAccumulator::new("s1");
    acc.append_goal("g", 1000);
    acc.append_tool_call("read", None, None, None, None, None, 1001);
    acc.append_tool_call("read", None, None, None, None, None, 1002);
    acc.append_tool_call("edit", None, None, None, None, None, 1003);

    let test_input = serde_json::json!({"command": "cargo test"});
    acc.append_tool_call("bash", None, Some(&test_input), None, None, None, 1004);

    let install_input = serde_json::json!({"command": "npm install"});
    acc.append_tool_call("bash", None, Some(&install_input), None, None, None, 1005);

    acc.append_tool_call("edit", None, None, None, Some("error"), None, 1006);
    acc.append_human_gate("proceed?", 1007);
    acc.append_patch_proposal("HASH", &[], 1008);

    let stats = acc.stats();
    assert_eq!(stats.goal_count, 1);
    assert_eq!(stats.exploration_count, 2);
    assert_eq!(stats.commitment_count, 1);
    assert_eq!(stats.verification_count, 1);
    assert_eq!(stats.execution_count, 1);
    assert_eq!(stats.error_count, 1);
    assert_eq!(stats.human_gate_count, 1);
    assert_eq!(stats.patch_proposal_count, 1);
    assert_eq!(stats.total_nodes(), acc.node_count() as u32);
    assert_eq!(stats.edge_count, acc.edge_count() as u32);
}
