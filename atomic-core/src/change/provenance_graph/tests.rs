//! Tests for the provenance graph module.

use super::*;
use crate::types::{Base32, Hash};

// ---- Helpers ----

fn make_goal(id: &str, summary: &str) -> ProvenanceNode {
    ProvenanceNode {
        id: id.into(),
        kind: ProvenanceNodeKind::Goal,
        timestamp: 1000,
        summary: summary.into(),
        detail: None,
        change_hash: None,
        tool_name: None,
        tool_call_id: None,
        duration_ms: None,
        classified: false,
        confidence: None,
        consolidated_from: Vec::new(),
    }
}

fn make_commitment(id: &str, summary: &str) -> ProvenanceNode {
    ProvenanceNode {
        id: id.into(),
        kind: ProvenanceNodeKind::Commitment,
        timestamp: 2000,
        summary: summary.into(),
        detail: None,
        change_hash: None,
        tool_name: Some("edit".into()),
        tool_call_id: Some("call-1".into()),
        duration_ms: Some(150),
        classified: false,
        confidence: None,
        consolidated_from: Vec::new(),
    }
}

fn make_exploration(id: &str, summary: &str) -> ProvenanceNode {
    ProvenanceNode {
        id: id.into(),
        kind: ProvenanceNodeKind::Exploration,
        timestamp: 1500,
        summary: summary.into(),
        detail: None,
        change_hash: None,
        tool_name: Some("read".into()),
        tool_call_id: None,
        duration_ms: None,
        classified: false,
        confidence: None,
        consolidated_from: Vec::new(),
    }
}

fn make_verification(id: &str, summary: &str) -> ProvenanceNode {
    ProvenanceNode {
        id: id.into(),
        kind: ProvenanceNodeKind::Verification,
        timestamp: 3000,
        summary: summary.into(),
        detail: None,
        change_hash: None,
        tool_name: Some("bash".into()),
        tool_call_id: None,
        duration_ms: Some(3200),
        classified: false,
        confidence: None,
        consolidated_from: Vec::new(),
    }
}

fn make_edge(from: &str, to: &str, kind: ProvenanceEdgeKind) -> ProvenanceEdge {
    ProvenanceEdge {
        from: from.into(),
        to: to.into(),
        kind,
    }
}

fn sample_graph() -> ProvenanceGraph {
    ProvenanceGraph::builder("sess-123", "claude-code")
        .agent_display_name("Claude Code")
        .agent_vendor("anthropic")
        .timestamp(1735689600)
        .add_node(make_goal("n-1", "Fix the auth bug"))
        .add_node(make_exploration("n-2", "Read src/auth.rs"))
        .add_node(make_exploration("n-3", "Read src/jwt.rs"))
        .add_node(make_commitment("n-4", "Edit src/auth.rs"))
        .add_node(make_verification("n-5", "cargo test --lib (passed)"))
        .add_edge(make_edge("n-1", "n-2", ProvenanceEdgeKind::LedTo))
        .add_edge(make_edge("n-1", "n-3", ProvenanceEdgeKind::LedTo))
        .add_edge(make_edge("n-2", "n-4", ProvenanceEdgeKind::ExploredVia))
        .add_edge(make_edge("n-3", "n-4", ProvenanceEdgeKind::ExploredVia))
        .add_edge(make_edge("n-4", "n-5", ProvenanceEdgeKind::VerifiedBy))
        .add_change_explained(Hash::of(b"change-a"))
        .build()
}

// ---- Builder ----

#[test]
fn test_builder_minimal() {
    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .timestamp(1000)
        .build();

    assert_eq!(graph.version, 2);
    assert_eq!(graph.timestamp, 1000);
    assert_eq!(graph.session_id, "sess-1");
    assert_eq!(graph.agent_name, "agent");
    assert!(graph.agent_display_name.is_empty());
    assert!(graph.agent_vendor.is_empty());
    assert!(graph.profile.is_none());
    assert!(graph.nodes.is_empty());
    assert!(graph.edges.is_empty());
    assert!(graph.changes_explained.is_empty());
    assert!(graph.previous.is_none());
    assert!(graph.stats.is_empty());
}

#[test]
fn test_builder_with_sherpa_profile() {
    let graph = ProvenanceGraph::builder("sess-1", "sherpa")
        .timestamp(1000)
        .profile(SHERPA_PROFILE)
        .build();

    assert_eq!(graph.profile, Some(SHERPA_PROFILE.to_string()));
}

#[test]
fn test_profile_none_by_default() {
    let graph = sample_graph();
    assert!(graph.profile.is_none());
}

#[test]
fn test_profile_roundtrips_through_serialization() {
    let graph = ProvenanceGraph::builder("sess-1", "sherpa")
        .timestamp(1000)
        .profile(SHERPA_PROFILE)
        .build();

    let bytes = graph.serialize().unwrap();
    let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

    assert_eq!(loaded.profile, Some(SHERPA_PROFILE.to_string()));
}

#[test]
fn test_profile_absent_on_old_graph_deserializes_as_none() {
    // Simulate a v1 payload: build a ProvenanceGraphV1 directly and
    // serialize it with postcard (no profile field), then wrap it with
    // the PRVG magic and verify that deserialize upgrades it to v2 with
    // profile = None.
    let v1 = ProvenanceGraphV1 {
        version: 1,
        timestamp: 500,
        session_id: "sess-old".into(),
        agent_name: "claude-code".into(),
        agent_display_name: String::new(),
        agent_vendor: String::new(),
        nodes: Vec::new(),
        edges: Vec::new(),
        changes_explained: Vec::new(),
        previous: None,
        stats: ProvenanceStats::default(),
    };

    let payload = postcard::to_allocvec(&v1).unwrap();
    let mut bytes = b"PRVG".to_vec();
    bytes.extend_from_slice(&payload);

    let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

    assert!(loaded.profile.is_none());
    // Version is upgraded to current schema version in memory.
    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.session_id, "sess-old");
}

#[test]
fn test_builder_full() {
    let graph = sample_graph();

    assert_eq!(graph.session_id, "sess-123");
    assert_eq!(graph.agent_name, "claude-code");
    assert_eq!(graph.agent_display_name, "Claude Code");
    assert_eq!(graph.agent_vendor, "anthropic");
    assert_eq!(graph.nodes.len(), 5);
    assert_eq!(graph.edges.len(), 5);
    assert_eq!(graph.changes_explained.len(), 1);
    assert!(graph.previous.is_none());
}

#[test]
fn test_builder_auto_computes_stats() {
    let graph = sample_graph();

    assert_eq!(graph.stats.goal_count, 1);
    assert_eq!(graph.stats.exploration_count, 2);
    assert_eq!(graph.stats.commitment_count, 1);
    assert_eq!(graph.stats.verification_count, 1);
    assert_eq!(graph.stats.edge_count, 5);
    assert_eq!(graph.stats.total_nodes(), 5);
}

#[test]
fn test_builder_with_previous() {
    let prev_hash = Hash::of(b"previous-graph");
    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .previous(prev_hash)
        .build();

    assert!(graph.is_chained());
    assert_eq!(graph.previous, Some(prev_hash));
}

#[test]
fn test_builder_with_changes_explained() {
    let h1 = Hash::of(b"change-1");
    let h2 = Hash::of(b"change-2");
    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .changes_explained(vec![h1, h2])
        .build();

    assert_eq!(graph.change_count(), 2);
    assert!(graph.explains_change(&h1));
    assert!(graph.explains_change(&h2));
    assert!(!graph.explains_change(&Hash::of(b"other")));
}

#[test]
fn test_builder_timestamp_defaults_to_now() {
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let graph = ProvenanceGraph::builder("sess-1", "agent").build();

    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    assert!(graph.timestamp >= before);
    assert!(graph.timestamp <= after);
}

// ---- Serialization ----

#[test]
fn test_serialize_has_magic() {
    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .timestamp(1000)
        .build();
    let bytes = graph.serialize().unwrap();

    assert!(bytes.len() > 4);
    assert_eq!(&bytes[..4], b"PRVG");
}

#[test]
fn test_is_provenance_graph() {
    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .timestamp(1000)
        .build();
    let bytes = graph.serialize().unwrap();

    assert!(ProvenanceGraph::is_provenance_graph(&bytes));
    assert!(!ProvenanceGraph::is_provenance_graph(b"ATST"));
    assert!(!ProvenanceGraph::is_provenance_graph(b"PRV"));
    assert!(!ProvenanceGraph::is_provenance_graph(b""));
    assert!(!ProvenanceGraph::is_provenance_graph(b"hello world"));
}

#[test]
fn test_serialize_deserialize_roundtrip_minimal() {
    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .timestamp(1000)
        .build();

    let bytes = graph.serialize().unwrap();
    let (loaded, hash) = ProvenanceGraph::deserialize(&bytes).unwrap();

    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.session_id, "sess-1");
    assert_eq!(loaded.agent_name, "agent");
    assert_eq!(loaded.timestamp, 1000);
    assert!(!hash.to_base32().is_empty());
}

#[test]
fn test_serialize_deserialize_roundtrip_full() {
    let graph = sample_graph();

    let bytes = graph.serialize().unwrap();
    let (loaded, _hash) = ProvenanceGraph::deserialize(&bytes).unwrap();

    assert_eq!(loaded.session_id, "sess-123");
    assert_eq!(loaded.agent_name, "claude-code");
    assert_eq!(loaded.agent_display_name, "Claude Code");
    assert_eq!(loaded.agent_vendor, "anthropic");
    assert_eq!(loaded.nodes.len(), 5);
    assert_eq!(loaded.edges.len(), 5);
    assert_eq!(loaded.changes_explained.len(), 1);
    assert_eq!(loaded.stats.goal_count, 1);
    assert_eq!(loaded.stats.exploration_count, 2);
    assert_eq!(loaded.stats.commitment_count, 1);
    assert_eq!(loaded.stats.verification_count, 1);
    assert_eq!(loaded.stats.edge_count, 5);
}

#[test]
fn test_serialize_deserialize_with_chaining() {
    let prev_hash = Hash::of(b"prev");
    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .timestamp(1000)
        .previous(prev_hash)
        .build();

    let bytes = graph.serialize().unwrap();
    let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

    assert!(loaded.is_chained());
    assert_eq!(loaded.previous, Some(prev_hash));
}

#[test]
fn test_serialize_deterministic() {
    let graph = sample_graph();

    let bytes1 = graph.serialize().unwrap();
    let bytes2 = graph.serialize().unwrap();

    assert_eq!(bytes1, bytes2);
    assert_eq!(Hash::of(&bytes1), Hash::of(&bytes2));
}

#[test]
fn test_deserialize_too_short() {
    let result = ProvenanceGraph::deserialize(b"PRV");
    assert!(result.is_err());
}

#[test]
fn test_deserialize_wrong_magic() {
    let result = ProvenanceGraph::deserialize(b"ATSTsomedata");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("invalid magic"));
}

#[test]
fn test_deserialize_corrupt_payload() {
    let mut data = Vec::new();
    data.extend_from_slice(b"PRVG");
    data.extend_from_slice(b"this is not valid postcard data");

    let result = ProvenanceGraph::deserialize(&data);
    assert!(result.is_err());
}

// ---- Read/Write ----

#[test]
fn test_write_read_roundtrip() {
    let graph = sample_graph();

    let mut buf = Vec::new();
    let write_hash = graph.write_to(&mut buf).unwrap();

    let (loaded, read_hash) = ProvenanceGraph::read_from(&mut buf.as_slice()).unwrap();

    assert_eq!(write_hash, read_hash);
    assert_eq!(loaded.session_id, "sess-123");
    assert_eq!(loaded.nodes.len(), 5);
}

// ---- Node queries ----

#[test]
fn test_find_node() {
    let graph = sample_graph();

    let node = graph.find_node("n-1").unwrap();
    assert_eq!(node.kind, ProvenanceNodeKind::Goal);
    assert_eq!(node.summary, "Fix the auth bug");

    assert!(graph.find_node("nonexistent").is_none());
}

#[test]
fn test_nodes_of_kind() {
    let graph = sample_graph();

    let goals = graph.nodes_of_kind(ProvenanceNodeKind::Goal);
    assert_eq!(goals.len(), 1);

    let explorations = graph.nodes_of_kind(ProvenanceNodeKind::Exploration);
    assert_eq!(explorations.len(), 2);

    let decisions = graph.nodes_of_kind(ProvenanceNodeKind::Decision);
    assert!(decisions.is_empty());
}

// ---- Edge queries ----

#[test]
fn test_edges_from() {
    let graph = sample_graph();

    let from_goal = graph.edges_from("n-1");
    assert_eq!(from_goal.len(), 2);
    assert!(from_goal
        .iter()
        .all(|e| e.kind == ProvenanceEdgeKind::LedTo));

    let from_commit = graph.edges_from("n-4");
    assert_eq!(from_commit.len(), 1);
    assert_eq!(from_commit[0].kind, ProvenanceEdgeKind::VerifiedBy);
}

#[test]
fn test_edges_to() {
    let graph = sample_graph();

    let to_commit = graph.edges_to("n-4");
    assert_eq!(to_commit.len(), 2);
    assert!(to_commit
        .iter()
        .all(|e| e.kind == ProvenanceEdgeKind::ExploredVia));
}

// ---- Backward traversal ----

#[test]
fn test_walk_backward_from_verification() {
    let graph = sample_graph();

    // Walk backward from the verification node
    let chain = graph.walk_backward("n-5");

    // Should include: n-5 (verification) ← n-4 (commitment) ← n-2, n-3 (explorations) ← n-1 (goal)
    assert!(chain.contains(&"n-5".to_string()));
    assert!(chain.contains(&"n-4".to_string()));
    assert!(chain.contains(&"n-2".to_string()));
    assert!(chain.contains(&"n-3".to_string()));
    assert!(chain.contains(&"n-1".to_string()));
    assert_eq!(chain.len(), 5);

    // First element should be the start node
    assert_eq!(chain[0], "n-5");
}

#[test]
fn test_walk_backward_from_goal() {
    let graph = sample_graph();

    // Walking backward from the goal — it's a root, so only itself
    let chain = graph.walk_backward("n-1");
    assert_eq!(chain, vec!["n-1"]);
}

#[test]
fn test_walk_backward_nonexistent() {
    let graph = sample_graph();

    let chain = graph.walk_backward("nonexistent");
    assert_eq!(chain, vec!["nonexistent"]);
}

// ---- Node types ----

#[test]
fn test_node_kind_serde_roundtrip() {
    let kinds = [
        ProvenanceNodeKind::Goal,
        ProvenanceNodeKind::Exploration,
        ProvenanceNodeKind::Decision,
        ProvenanceNodeKind::Commitment,
        ProvenanceNodeKind::Verification,
        ProvenanceNodeKind::Execution,
        ProvenanceNodeKind::HumanGate,
        ProvenanceNodeKind::PatchProposal,
        ProvenanceNodeKind::Error,
    ];
    for kind in &kinds {
        let json = serde_json::to_string(kind).unwrap();
        let back: ProvenanceNodeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(*kind, back);
    }
}

#[test]
fn test_node_kind_snake_case() {
    assert_eq!(
        serde_json::to_string(&ProvenanceNodeKind::HumanGate).unwrap(),
        "\"human_gate\""
    );
    assert_eq!(
        serde_json::to_string(&ProvenanceNodeKind::PatchProposal).unwrap(),
        "\"patch_proposal\""
    );
}

#[test]
fn test_edge_kind_serde_roundtrip() {
    let kinds = [
        ProvenanceEdgeKind::LedTo,
        ProvenanceEdgeKind::ExploredVia,
        ProvenanceEdgeKind::CommittedVia,
        ProvenanceEdgeKind::VerifiedBy,
        ProvenanceEdgeKind::BlockedBy,
        ProvenanceEdgeKind::ResumedAfter,
        ProvenanceEdgeKind::FailedWith,
    ];
    for kind in &kinds {
        let json = serde_json::to_string(kind).unwrap();
        let back: ProvenanceEdgeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(*kind, back);
    }
}

// ---- ProvenanceStats ----

#[test]
fn test_stats_from_graph() {
    let nodes = vec![
        make_goal("n-1", "Goal"),
        make_exploration("n-2", "Read"),
        make_commitment("n-3", "Edit"),
    ];
    let edges = vec![
        make_edge("n-1", "n-2", ProvenanceEdgeKind::LedTo),
        make_edge("n-2", "n-3", ProvenanceEdgeKind::ExploredVia),
    ];

    let stats = ProvenanceStats::from_graph(&nodes, &edges);

    assert_eq!(stats.goal_count, 1);
    assert_eq!(stats.exploration_count, 1);
    assert_eq!(stats.commitment_count, 1);
    assert_eq!(stats.edge_count, 2);
    assert_eq!(stats.total_nodes(), 3);
    assert!(!stats.is_empty());
}

#[test]
fn test_stats_default_is_empty() {
    let stats = ProvenanceStats::default();
    assert!(stats.is_empty());
    assert_eq!(stats.total_nodes(), 0);
}

#[test]
fn test_stats_display() {
    let mut stats = ProvenanceStats::default();
    stats.goal_count = 1;
    stats.commitment_count = 2;
    stats.edge_count = 3;

    let display = format!("{}", stats);
    assert!(display.contains("1 goal"));
    assert!(display.contains("2 commitments"));
    assert!(display.contains("3 edges"));
}

#[test]
fn test_stats_display_empty() {
    let stats = ProvenanceStats::default();
    assert_eq!(format!("{}", stats), "empty graph");
}

// ---- Display ----

#[test]
fn test_graph_display() {
    let graph = sample_graph();
    let display = format!("{}", graph);

    assert!(display.contains("Claude Code"));
    assert!(display.contains("5 nodes"));
    assert!(display.contains("5 edges"));
    assert!(display.contains("Fix the auth bug"));
}

#[test]
fn test_node_display() {
    let node = make_goal("n-1", "Fix the bug");
    let display = format!("{}", node);

    assert!(display.contains("n-1"));
    assert!(display.contains("goal"));
    assert!(display.contains("Fix the bug"));
}

#[test]
fn test_edge_display() {
    let edge = make_edge("a", "b", ProvenanceEdgeKind::LedTo);
    let display = format!("{}", edge);

    assert!(display.contains("a"));
    assert!(display.contains("b"));
    assert!(display.contains("led_to"));
}

// ---- Node detail ----

#[test]
fn test_node_with_detail() {
    let mut node = make_commitment("n-1", "Edit file");
    node.detail = Some(r#"{"file":"src/auth.rs","tool":"edit"}"#.into());

    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .timestamp(1000)
        .add_node(node)
        .build();

    let bytes = graph.serialize().unwrap();
    let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

    let loaded_node = &loaded.nodes[0];
    assert!(loaded_node.detail.is_some());
    assert!(loaded_node.detail.as_ref().unwrap().contains("src/auth.rs"));
}

#[test]
fn test_node_with_change_hash() {
    let hash = Hash::of(b"my-change");
    let mut node = make_commitment("n-1", "Edit file");
    node.change_hash = Some(hash);

    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .timestamp(1000)
        .add_node(node)
        .build();

    let bytes = graph.serialize().unwrap();
    let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

    assert_eq!(loaded.nodes[0].change_hash, Some(hash));
}

#[test]
fn test_node_classified_fields() {
    let mut node = make_goal("n-1", "Explored auth → chose JWT fix");
    node.kind = ProvenanceNodeKind::Decision;
    node.classified = true;
    node.confidence = Some(0.92);
    node.consolidated_from = vec!["n-2".into(), "n-3".into(), "n-4".into()];

    let graph = ProvenanceGraph::builder("sess-1", "agent")
        .timestamp(1000)
        .add_node(node)
        .build();

    let bytes = graph.serialize().unwrap();
    let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

    let loaded_node = &loaded.nodes[0];
    assert!(loaded_node.classified);
    assert!((loaded_node.confidence.unwrap() - 0.92).abs() < 0.001);
    assert_eq!(loaded_node.consolidated_from, vec!["n-2", "n-3", "n-4"]);
}

// ---- Error type ----

#[test]
fn test_error_display_codec() {
    let err = ProvenanceGraphError::Codec {
        reason: "bad data".into(),
    };
    assert!(err.to_string().contains("bad data"));
}

#[test]
fn test_error_display_version() {
    let err = ProvenanceGraphError::UnsupportedVersion {
        version: 99,
        max_supported: 1,
    };
    let display = err.to_string();
    assert!(display.contains("99"));
    assert!(display.contains("1"));
}

// ---- Size / performance ----

#[test]
fn test_serialized_size_reasonable() {
    let graph = sample_graph();
    let bytes = graph.serialize().unwrap();

    // A 5-node graph should serialize to a few hundred bytes, not kilobytes
    assert!(bytes.len() < 2000, "serialized size: {} bytes", bytes.len());
    assert!(
        bytes.len() > MAGIC.len(),
        "serialized size should exceed magic"
    );
}

#[test]
fn test_large_graph_serializes() {
    let mut builder = ProvenanceGraph::builder("sess-large", "agent").timestamp(1000);

    // Build a 100-node graph
    for i in 0..100 {
        builder = builder.add_node(ProvenanceNode {
            id: format!("n-{}", i),
            kind: if i == 0 {
                ProvenanceNodeKind::Goal
            } else if i % 3 == 0 {
                ProvenanceNodeKind::Exploration
            } else if i % 3 == 1 {
                ProvenanceNodeKind::Commitment
            } else {
                ProvenanceNodeKind::Verification
            },
            timestamp: 1000 + i as i64,
            summary: format!("Node {}", i),
            detail: None,
            change_hash: None,
            tool_name: Some("tool".into()),
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        });

        if i > 0 {
            builder = builder.add_edge(ProvenanceEdge {
                from: format!("n-{}", i - 1),
                to: format!("n-{}", i),
                kind: ProvenanceEdgeKind::LedTo,
            });
        }
    }

    let graph = builder.build();
    assert_eq!(graph.node_count(), 100);
    assert_eq!(graph.edge_count(), 99);

    let bytes = graph.serialize().unwrap();
    let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();
    assert_eq!(loaded.node_count(), 100);

    // 100 nodes should still be well under 50KB
    assert!(
        bytes.len() < 50_000,
        "100-node graph serialized to {} bytes",
        bytes.len()
    );
}

// ---- Cross-compatibility with serde_json ----

#[test]
fn test_node_kind_compatible_with_agent_types() {
    // Verify that the JSON representation matches the agent-side types.
    // The agent uses serde_json for graph.json; this crate uses postcard
    // for .provenance files. But the enum variants must match for any
    // JSON-based interchange.
    let kinds = [
        ("goal", ProvenanceNodeKind::Goal),
        ("exploration", ProvenanceNodeKind::Exploration),
        ("decision", ProvenanceNodeKind::Decision),
        ("commitment", ProvenanceNodeKind::Commitment),
        ("verification", ProvenanceNodeKind::Verification),
        ("execution", ProvenanceNodeKind::Execution),
        ("human_gate", ProvenanceNodeKind::HumanGate),
        ("patch_proposal", ProvenanceNodeKind::PatchProposal),
        ("error", ProvenanceNodeKind::Error),
    ];

    for (expected_json, kind) in &kinds {
        let json = serde_json::to_string(kind).unwrap();
        assert_eq!(
            json,
            format!("\"{}\"", expected_json),
            "NodeKind::{:?} JSON mismatch",
            kind
        );
    }
}

#[test]
fn test_edge_kind_compatible_with_agent_types() {
    let kinds = [
        ("led_to", ProvenanceEdgeKind::LedTo),
        ("explored_via", ProvenanceEdgeKind::ExploredVia),
        ("committed_via", ProvenanceEdgeKind::CommittedVia),
        ("verified_by", ProvenanceEdgeKind::VerifiedBy),
        ("blocked_by", ProvenanceEdgeKind::BlockedBy),
        ("resumed_after", ProvenanceEdgeKind::ResumedAfter),
        ("failed_with", ProvenanceEdgeKind::FailedWith),
    ];

    for (expected_json, kind) in &kinds {
        let json = serde_json::to_string(kind).unwrap();
        assert_eq!(
            json,
            format!("\"{}\"", expected_json),
            "EdgeKind::{:?} JSON mismatch",
            kind
        );
    }
}
