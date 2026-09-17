use std::collections::BTreeMap;

use crate::event::{
    canonicalize_provenance_journal, ProvenanceJournalEnvelope, ProvenanceJournalError,
    ProvenanceJournalEvent, ProvenanceToolPhase,
};
use crate::provenance::accumulator::helpers::build_tool_detail;
use crate::provenance::classify::{classify_tool_call, summarize_tool_call};
use crate::provenance::types::{GraphEdge, GraphStats, NodeKind};

use super::ProvenanceAccumulator;

impl ProvenanceAccumulator {
    /// Replay a complete turn journal into deterministic graph semantics.
    ///
    /// Arrival order is discarded by `canonicalize_provenance_journal`; causal
    /// parents and the stable ordering key determine all accumulator mutations.
    pub fn replay_provenance_journal(
        envelopes: Vec<ProvenanceJournalEnvelope>,
    ) -> Result<Self, ProvenanceJournalError> {
        let ordered = canonicalize_provenance_journal(envelopes)?;
        let Some(first) = ordered.first() else {
            return Ok(Self::new(""));
        };
        let mut accumulator = Self::new(&first.session_id);
        let mut event_nodes = BTreeMap::<String, String>::new();

        for envelope in ordered {
            let timestamp = envelope.timestamp_ms;
            let node_id =
                match envelope.event {
                    ProvenanceJournalEvent::Goal { prompt } => {
                        Some(accumulator.append_goal(&prompt, timestamp))
                    }
                    ProvenanceJournalEvent::Tool {
                        phase: ProvenanceToolPhase::Before,
                        ..
                    } => None,
                    ProvenanceJournalEvent::Tool {
                        phase: ProvenanceToolPhase::After,
                        tool_name,
                        tool_call_id,
                        input,
                        output,
                        status,
                        duration_ms,
                        ..
                    } => {
                        let output_text = output.as_ref().map(json_value_as_text);
                        Some(accumulator.append_tool_call(
                            &tool_name,
                            tool_call_id.as_deref(),
                            input.as_ref(),
                            output_text.as_deref(),
                            status.as_deref(),
                            duration_ms,
                            timestamp,
                        ))
                    }
                    ProvenanceJournalEvent::ToolEnrichment {
                        tool_call_id,
                        tool_name,
                        input,
                        output,
                        status,
                    } => {
                        let kind = classify_tool_call(
                            &tool_name,
                            input.as_ref(),
                            output.as_deref(),
                            status.as_deref(),
                        );
                        let summary = summarize_tool_call(
                            &tool_name,
                            kind,
                            input.as_ref(),
                            output.as_deref(),
                            status.as_deref(),
                        );
                        if let Some(node) = accumulator.nodes.iter_mut().find(|node| {
                            node.tool_call_id.as_deref() == Some(tool_call_id.as_str())
                        }) {
                            node.kind = kind;
                            node.summary = summary;
                            node.tool_name = Some(tool_name.clone());
                            node.detail = build_tool_detail(
                                kind,
                                &tool_name,
                                input.as_ref(),
                                output.as_deref(),
                                status.as_deref(),
                            );
                            None
                        } else {
                            Some(accumulator.append_tool_call(
                                &tool_name,
                                Some(&tool_call_id),
                                input.as_ref(),
                                output.as_deref(),
                                status.as_deref(),
                                None,
                                timestamp,
                            ))
                        }
                    }
                    ProvenanceJournalEvent::Reasoning {
                        text,
                        duration_ms,
                        signature,
                    } => Some(accumulator.append_reasoning(
                        &text,
                        duration_ms,
                        signature.as_deref(),
                        timestamp,
                    )),
                    ProvenanceJournalEvent::Response { text } => {
                        Some(accumulator.append_llm_response(&text, timestamp))
                    }
                    ProvenanceJournalEvent::Todo { todo } => {
                        Some(accumulator.append_todo_snapshot(&todo, timestamp))
                    }
                    ProvenanceJournalEvent::HumanGate { reason } => {
                        Some(accumulator.append_human_gate(&reason, timestamp))
                    }
                    ProvenanceJournalEvent::HumanGateResolution {
                        gate_event_id,
                        resolution: _,
                        detail: _,
                    } => {
                        let gate_node = event_nodes
                            .get(&gate_event_id)
                            .ok_or(ProvenanceJournalError::MissingHumanGate(gate_event_id))?;
                        accumulator.resolve_human_gate(gate_node);
                        None
                    }
                    ProvenanceJournalEvent::PatchProposal { change_hash, files } => {
                        Some(accumulator.append_patch_proposal(&change_hash, &files, timestamp))
                    }
                    ProvenanceJournalEvent::Terminal { .. } => None,
                    ProvenanceJournalEvent::GraphDelta { nodes, edges }
                    | ProvenanceJournalEvent::LegacyGraphImport { nodes, edges, .. } => {
                        accumulator.apply_graph_delta(nodes, edges)?;
                        None
                    }
                };
            if let Some(node_id) = node_id {
                event_nodes.insert(envelope.event_id, node_id);
            }
        }

        accumulator.rebuild_replay_state();
        Ok(accumulator)
    }

    /// Return only graph state not acknowledged by a prior publication.
    pub fn pending_graph_delta(&self) -> Option<ProvenanceJournalEvent> {
        let nodes = self.nodes.get(self.nodes_saved_count..)?.to_vec();
        let pending_ids: std::collections::HashSet<_> =
            nodes.iter().map(|node| node.id.as_str()).collect();
        let edges = self
            .edges
            .get(self.edges_saved_count..)?
            .iter()
            .filter(|edge| {
                pending_ids.contains(edge.from.as_str()) || pending_ids.contains(edge.to.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        if nodes.is_empty() && edges.is_empty() {
            None
        } else {
            Some(ProvenanceJournalEvent::LegacyGraphImport {
                nodes,
                edges,
                previous_provenance: self.last_provenance_hash.clone(),
            })
        }
    }

    /// Capture the current graph as one lossless compatibility envelope.
    ///
    /// New producers should prefer typed journal events so raw tool and model
    /// data remain available. This adapter exists to migrate existing graphs
    /// without changing any node or edge semantics.
    pub fn to_journal_snapshot(
        &self,
        turn_number: u32,
        generation: u64,
        timestamp_ms: i64,
    ) -> Result<ProvenanceJournalEnvelope, ProvenanceJournalError> {
        let event = ProvenanceJournalEvent::GraphDelta {
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
        };
        let event_id = ProvenanceJournalEnvelope::deterministic_id(
            &self.session_id,
            turn_number,
            generation,
            timestamp_ms,
            &event,
        )?;
        Ok(ProvenanceJournalEnvelope::new(
            event_id,
            &self.session_id,
            turn_number,
            generation,
            timestamp_ms,
            event,
        ))
    }

    fn apply_graph_delta(
        &mut self,
        nodes: Vec<crate::provenance::types::GraphNode>,
        edges: Vec<GraphEdge>,
    ) -> Result<(), ProvenanceJournalError> {
        for node in nodes {
            match self.nodes.iter().find(|existing| existing.id == node.id) {
                Some(existing) if existing == &node => {}
                Some(_) => {
                    return Err(ProvenanceJournalError::ConflictingGraphNode(node.id));
                }
                None => self.nodes.push(node),
            }
        }
        for edge in edges {
            if !self.edges.contains(&edge) {
                self.edges.push(edge);
            }
        }
        Ok(())
    }

    fn rebuild_replay_state(&mut self) {
        self.stats = GraphStats::default();
        for node in &self.nodes {
            self.stats.increment(node.kind);
        }
        self.stats.edge_count = self.edges.len() as u32;
        self.counter = self
            .nodes
            .iter()
            .filter_map(|node| node.id.rsplit('-').next()?.parse::<u64>().ok())
            .max()
            .unwrap_or(self.nodes.len() as u64);
        self.last_node = self.nodes.last().map(|node| node.id.clone());
        self.current_goal = self
            .nodes
            .iter()
            .rev()
            .find(|node| node.kind == NodeKind::Goal)
            .map(|node| node.id.clone());
        self.last_commitment = self
            .nodes
            .iter()
            .rev()
            .find(|node| node.kind == NodeKind::Commitment)
            .map(|node| node.id.clone());
        self.pending_human_gate = self
            .nodes
            .iter()
            .rev()
            .find(|node| {
                node.kind == NodeKind::HumanGate
                    && node
                        .detail
                        .as_ref()
                        .and_then(|detail| detail.get("resolved"))
                        .and_then(serde_json::Value::as_bool)
                        != Some(true)
            })
            .map(|node| node.id.clone());

        self.pending_explorations.clear();
        for node in self.nodes.iter().rev() {
            if matches!(node.kind, NodeKind::Goal | NodeKind::Commitment) {
                break;
            }
            if node.kind == NodeKind::Exploration {
                self.pending_explorations.push(node.id.clone());
            }
        }
        self.pending_explorations.reverse();

        self.commitments_since_last_patch.clear();
        for node in self.nodes.iter().rev() {
            if node.kind == NodeKind::PatchProposal {
                break;
            }
            if node.kind == NodeKind::Commitment {
                self.commitments_since_last_patch.push(node.id.clone());
            }
        }
        self.commitments_since_last_patch.reverse();
        self.nodes_saved_count = 0;
        self.edges_saved_count = 0;
    }
}

fn json_value_as_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use atomic_core::change::session::SessionTodo;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::event::{HookType, ProvenanceToolPhase, TurnEvent};
    use crate::provenance::types::{GraphNode, NodeKind};

    fn envelope(
        id: &str,
        timestamp_ms: i64,
        parents: &[&str],
        event: ProvenanceJournalEvent,
    ) -> ProvenanceJournalEnvelope {
        ProvenanceJournalEnvelope::new(id, "session-1", 3, 2, timestamp_ms, event)
            .with_causal_parents(parents.iter().copied())
    }

    #[test]
    fn all_payloads_round_trip_without_loss() {
        let node = GraphNode {
            id: "external-9".to_string(),
            kind: NodeKind::Decision,
            timestamp: 17,
            summary: "choose exact replay".to_string(),
            detail: Some(json!({"nested": [1, true, {"raw": "value"}]})),
            change_hash: Some("HASH".to_string()),
            tool_name: Some("planner".to_string()),
            tool_call_id: Some("call-9".to_string()),
            duration_ms: Some(44),
            classified: true,
            confidence: Some(0.75),
            consolidated_from: vec!["raw-1".to_string()],
        };
        let payloads = vec![
            ProvenanceJournalEvent::Goal {
                prompt: "full untruncated goal".to_string(),
            },
            ProvenanceJournalEvent::Tool {
                phase: ProvenanceToolPhase::After,
                tool_name: "terminal".to_string(),
                tool_call_id: Some("tool-1".to_string()),
                input: Some(json!({"command": "cargo test", "env": {"A": "B"}})),
                output: Some(json!({"stdout": "ok", "exit": 0})),
                status: Some("completed".to_string()),
                duration_ms: Some(123),
                raw: Some(json!({"vendor_extension": [1, 2, 3]})),
            },
            ProvenanceJournalEvent::Reasoning {
                text: "complete reasoning text".to_string(),
                duration_ms: Some(55),
                signature: Some("signature".to_string()),
            },
            ProvenanceJournalEvent::Response {
                text: "complete response".to_string(),
            },
            ProvenanceJournalEvent::Todo {
                todo: SessionTodo {
                    id: "todo-1".to_string(),
                    content: "finish replay".to_string(),
                    status: "in_progress".to_string(),
                    priority: "high".to_string(),
                },
            },
            ProvenanceJournalEvent::HumanGate {
                reason: "confirm migration".to_string(),
            },
            ProvenanceJournalEvent::HumanGateResolution {
                gate_event_id: "gate-1".to_string(),
                resolution: "approved".to_string(),
                detail: Some(json!({"command": "continue"})),
            },
            ProvenanceJournalEvent::PatchProposal {
                change_hash: "CHANGE".to_string(),
                files: vec!["src/lib.rs".to_string()],
            },
            ProvenanceJournalEvent::Terminal {
                hook_type: HookType::TurnEnd,
                reason: Some("completed".to_string()),
                payload: Some(json!({"stop_reason": "end_turn"})),
            },
            ProvenanceJournalEvent::GraphDelta {
                nodes: vec![node.clone()],
                edges: Vec::new(),
            },
            ProvenanceJournalEvent::LegacyGraphImport {
                nodes: vec![node],
                edges: Vec::new(),
                previous_provenance: Some("PREVIOUS".to_string()),
            },
        ];

        for (index, payload) in payloads.into_iter().enumerate() {
            let original = envelope(&format!("event-{index}"), index as i64, &[], payload);
            let bytes = original.to_json_bytes().unwrap();
            let decoded = ProvenanceJournalEnvelope::from_json_bytes(&bytes).unwrap();
            assert_eq!(decoded, original);
        }
    }

    #[test]
    fn legacy_version_alias_and_generation_default_decode() {
        let bytes = br#"{
            "version": 1,
            "event_id": "legacy-1",
            "session_id": "session-1",
            "turn_number": 3,
            "timestamp_ms": 1000,
            "event": {"type": "goal", "prompt": "legacy goal"}
        }"#;
        let decoded = ProvenanceJournalEnvelope::from_json_bytes(bytes).unwrap();
        assert_eq!(decoded.generation, 1);
        assert!(decoded.causal_parent_ids.is_empty());
    }

    #[test]
    fn turn_event_adapter_preserves_tool_payload_and_timestamp() {
        let timestamp = Utc.timestamp_millis_opt(1_700_000_000_123).unwrap();
        let turn_event = TurnEvent::new("session-1", HookType::PostToolUse)
            .with_tool_name("terminal")
            .with_tool_use_id("call-1")
            .with_timestamp(timestamp)
            .with_raw_json(json!({
                "tool_input": {"command": "cargo test"},
                "tool_output": {"stdout": "ok", "exit": 0},
                "status": "completed",
                "duration_ms": 42,
                "vendor": {"opaque": true}
            }));
        let envelope = ProvenanceJournalEnvelope::from_turn_event("hook-1", 3, 2, &turn_event);
        assert_eq!(envelope.timestamp_ms, 1_700_000_000_123);
        match envelope.event {
            ProvenanceJournalEvent::Tool {
                phase,
                input,
                output,
                raw,
                ..
            } => {
                assert_eq!(phase, ProvenanceToolPhase::After);
                assert_eq!(input, Some(json!({"command": "cargo test"})));
                assert_eq!(output, Some(json!({"stdout": "ok", "exit": 0})));
                assert_eq!(raw.unwrap()["vendor"]["opaque"], true);
            }
            other => panic!("unexpected adapter payload: {other:?}"),
        }
    }

    #[test]
    fn tool_enrichment_updates_the_correlated_node_without_duplication() {
        let tool = envelope(
            "tool",
            1,
            &[],
            ProvenanceJournalEvent::Tool {
                phase: ProvenanceToolPhase::After,
                tool_name: "bash".to_string(),
                tool_call_id: Some("call-1".to_string()),
                input: None,
                output: None,
                status: Some("completed".to_string()),
                duration_ms: None,
                raw: None,
            },
        );
        let enrichment = envelope(
            "enrichment",
            2,
            &["tool"],
            ProvenanceJournalEvent::ToolEnrichment {
                tool_call_id: "call-1".to_string(),
                tool_name: "bash".to_string(),
                input: Some(json!({"command": "cargo test"})),
                output: Some("test result: ok. 12 passed".to_string()),
                status: Some("completed".to_string()),
            },
        );
        let replayed =
            ProvenanceAccumulator::replay_provenance_journal(vec![enrichment, tool]).unwrap();
        assert_eq!(replayed.nodes().len(), 1);
        let node = &replayed.nodes()[0];
        assert!(node.summary.contains("cargo test"));
        assert_eq!(node.detail.as_ref().unwrap()["command"], "cargo test");
        assert!(node.detail.as_ref().unwrap()["output_summary"]
            .as_str()
            .unwrap()
            .contains("12 passed"));
    }

    #[test]
    fn deterministic_id_binds_complete_turn_identity_and_body() {
        let event = ProvenanceJournalEvent::Response {
            text: "done".to_string(),
        };
        let first =
            ProvenanceJournalEnvelope::deterministic_id("session-1", 3, 2, 1000, &event).unwrap();
        let retry =
            ProvenanceJournalEnvelope::deterministic_id("session-1", 3, 2, 1000, &event).unwrap();
        let next_generation =
            ProvenanceJournalEnvelope::deterministic_id("session-1", 3, 3, 1000, &event).unwrap();
        assert_eq!(first, retry);
        assert_ne!(first, next_generation);
    }

    #[test]
    fn replay_is_identical_for_all_arrival_permutations() {
        let events = vec![
            envelope(
                "goal",
                1000,
                &[],
                ProvenanceJournalEvent::Goal {
                    prompt: "implement deterministic replay".to_string(),
                },
            ),
            envelope(
                "before-read",
                1001,
                &["goal"],
                ProvenanceJournalEvent::Tool {
                    phase: ProvenanceToolPhase::Before,
                    tool_name: "read".to_string(),
                    tool_call_id: Some("read-1".to_string()),
                    input: Some(json!({"path": "src/lib.rs"})),
                    output: None,
                    status: None,
                    duration_ms: None,
                    raw: None,
                },
            ),
            envelope(
                "after-read",
                1002,
                &["before-read"],
                ProvenanceJournalEvent::Tool {
                    phase: ProvenanceToolPhase::After,
                    tool_name: "read".to_string(),
                    tool_call_id: Some("read-1".to_string()),
                    input: Some(json!({"path": "src/lib.rs"})),
                    output: Some(json!("contents")),
                    status: Some("completed".to_string()),
                    duration_ms: Some(2),
                    raw: None,
                },
            ),
            envelope(
                "reasoning",
                1003,
                &["after-read"],
                ProvenanceJournalEvent::Reasoning {
                    text: "choose an explicit envelope".to_string(),
                    duration_ms: Some(3),
                    signature: Some("sig".to_string()),
                },
            ),
            envelope(
                "response",
                1004,
                &["reasoning"],
                ProvenanceJournalEvent::Response {
                    text: "implemented".to_string(),
                },
            ),
            envelope(
                "terminal",
                1005,
                &["response"],
                ProvenanceJournalEvent::Terminal {
                    hook_type: HookType::TurnEnd,
                    reason: Some("complete".to_string()),
                    payload: Some(json!({"tokens": 10})),
                },
            ),
        ];
        let baseline = ProvenanceAccumulator::replay_provenance_journal(events.clone()).unwrap();

        let mut permutations = Vec::new();
        let mut candidate = events;
        collect_permutations(&mut candidate, 0, &mut permutations);
        assert_eq!(permutations.len(), 720);
        for permutation in permutations {
            let replayed = ProvenanceAccumulator::replay_provenance_journal(permutation).unwrap();
            assert_eq!(replayed.nodes(), baseline.nodes());
            assert_eq!(replayed.edges(), baseline.edges());
            assert_eq!(replayed.stats(), baseline.stats());
        }
    }

    #[test]
    fn graph_snapshot_adapter_replays_exact_semantics() {
        let mut original = ProvenanceAccumulator::new("session-1");
        original.append_goal("preserve graph", 1000);
        original.append_tool_call(
            "edit",
            Some("edit-1"),
            Some(&json!({"path": "src/lib.rs"})),
            Some("ok"),
            Some("completed"),
            Some(5),
            1001,
        );
        original.append_patch_proposal("HASH", &["src/lib.rs".to_string()], 1002);

        let snapshot = original.to_journal_snapshot(3, 2, 1003).unwrap();
        let replayed = ProvenanceAccumulator::replay_provenance_journal(vec![snapshot]).unwrap();
        assert_eq!(replayed.nodes(), original.nodes());
        assert_eq!(replayed.edges(), original.edges());
        assert_eq!(replayed.stats(), original.stats());
    }

    #[test]
    fn replay_rejects_missing_parents_and_cycles() {
        let missing = envelope(
            "child",
            1,
            &["missing"],
            ProvenanceJournalEvent::Response {
                text: "never replayed".to_string(),
            },
        );
        assert!(matches!(
            ProvenanceAccumulator::replay_provenance_journal(vec![missing]),
            Err(ProvenanceJournalError::MissingCausalParent { .. })
        ));

        let a = envelope(
            "a",
            1,
            &["b"],
            ProvenanceJournalEvent::Response {
                text: "a".to_string(),
            },
        );
        let b = envelope(
            "b",
            1,
            &["a"],
            ProvenanceJournalEvent::Response {
                text: "b".to_string(),
            },
        );
        assert!(matches!(
            ProvenanceAccumulator::replay_provenance_journal(vec![a, b]),
            Err(ProvenanceJournalError::CausalCycle)
        ));
    }

    fn collect_permutations(
        values: &mut [ProvenanceJournalEnvelope],
        start: usize,
        output: &mut Vec<Vec<ProvenanceJournalEnvelope>>,
    ) {
        if start == values.len() {
            output.push(values.to_vec());
            return;
        }
        for index in start..values.len() {
            values.swap(start, index);
            collect_permutations(values, start + 1, output);
            values.swap(start, index);
        }
    }
}
