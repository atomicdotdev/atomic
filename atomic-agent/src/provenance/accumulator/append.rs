use super::helpers::{build_tool_detail, short_hash, truncate_prompt};
use super::{ProvenanceAccumulator, MAX_GOAL_PROMPT_LEN, MAX_RESPONSE_TEXT_LEN};
use crate::provenance::classify::{classify_tool_call, summarize_tool_call};
use crate::provenance::types::{EdgeKind, GraphEdge, GraphNode, NodeKind};

impl ProvenanceAccumulator {
    // =========================================================================
    // Append methods
    // =========================================================================

    /// Append a goal node (human prompt).
    ///
    /// Returns the new node's ID.
    pub fn append_goal(&mut self, prompt: &str, timestamp: i64) -> String {
        let summary = truncate_prompt(prompt, MAX_GOAL_PROMPT_LEN);
        let node = GraphNode::new(self.next_id(), NodeKind::Goal, timestamp, &summary);
        let node_id = node.id.clone();

        // Edge inference: chain goals sequentially
        if let Some(ref prev_goal) = self.current_goal {
            self.edges.push(GraphEdge::new(
                prev_goal.clone(),
                node_id.clone(),
                EdgeKind::LedTo,
            ));
            self.stats.edge_count += 1;
        }

        // If there was a pending human gate, the new goal resumes after it
        if let Some(ref gate_id) = self.pending_human_gate.take() {
            self.edges.push(GraphEdge::new(
                gate_id.clone(),
                node_id.clone(),
                EdgeKind::ResumedAfter,
            ));
            self.stats.edge_count += 1;
        }

        // Reset cursor state for new goal
        self.current_goal = Some(node_id.clone());
        self.pending_explorations.clear();
        self.last_commitment = None;

        self.push_node(node);
        node_id
    }

    /// Append a tool call node, classified by the rule-based classifier.
    ///
    /// This is the primary entry point called from the orchestrator's
    /// `handle_tool_use` for `PostToolUse` events.
    ///
    /// Returns the new node's ID.
    #[allow(clippy::too_many_arguments)]
    pub fn append_tool_call(
        &mut self,
        tool_name: &str,
        tool_call_id: Option<&str>,
        tool_input: Option<&serde_json::Value>,
        tool_output: Option<&str>,
        status: Option<&str>,
        duration_ms: Option<u64>,
        timestamp: i64,
    ) -> String {
        let kind = classify_tool_call(tool_name, tool_input, tool_output, status);
        let summary = summarize_tool_call(tool_name, kind, tool_input, tool_output, status);

        let mut node =
            GraphNode::new(self.next_id(), kind, timestamp, summary).with_tool_name(tool_name);

        if let Some(id) = tool_call_id {
            node = node.with_tool_call_id(id);
        }
        if let Some(ms) = duration_ms {
            node = node.with_duration_ms(ms);
        }

        // Attach detail based on kind
        node.detail = build_tool_detail(kind, tool_name, tool_input, tool_output);

        let node_id = node.id.clone();

        // Infer edges based on the classified kind
        self.infer_edges(&node_id, kind);

        self.push_node(node);
        node_id
    }

    /// Append a reasoning/thinking node (chain-of-thought from the model).
    ///
    /// These are created from the reasoning blocks captured by the OpenCode
    /// plugin. Each block represents a distinct thinking step where the agent
    /// planned its approach, evaluated alternatives, or reasoned about the
    /// codebase.
    ///
    /// The node is classified as `Decision` (the existing kind for strategic
    /// choices). Edges link from the current goal to the reasoning node, and
    /// from the reasoning node to subsequent commitments/explorations.
    ///
    /// Returns the new node's ID.
    pub fn append_reasoning(
        &mut self,
        text: &str,
        duration_ms: Option<u64>,
        signature: Option<&str>,
        timestamp: i64,
    ) -> String {
        // Truncate for summary: first line or first 100 chars
        let first_line = text.lines().next().unwrap_or(text);
        let summary = if first_line.len() > 100 {
            let truncated: String = first_line.chars().take(97).collect();
            format!("{}...", truncated)
        } else {
            first_line.to_string()
        };

        let mut node = GraphNode::new(self.next_id(), NodeKind::Decision, timestamp, &summary);

        if let Some(ms) = duration_ms {
            node = node.with_duration_ms(ms);
        }

        // Build detail with the full reasoning text and signature
        let mut detail = serde_json::json!({
            "reasoning_text": text,
        });
        if let Some(ms) = duration_ms {
            detail["reasoning_duration_ms"] = serde_json::Value::Number(ms.into());
        }
        if let Some(sig) = signature {
            detail["anthropic_signature"] = serde_json::Value::String(sig.to_string());
        }
        detail["text_length"] = serde_json::Value::Number(text.len().into());
        node.detail = Some(detail);

        // Mark as classified so the Phase 3 consolidator doesn't touch it
        node.classified = true;
        node.confidence = Some(1.0);

        let node_id = node.id.clone();

        // Edge: goal --led_to-→ reasoning (if we have a current goal)
        if let Some(ref goal) = self.current_goal {
            self.edges.push(GraphEdge::new(
                goal.clone(),
                node_id.clone(),
                EdgeKind::LedTo,
            ));
            self.stats.edge_count += 1;
        }

        // Also chain from previous node for temporal ordering
        if let Some(ref prev) = self.last_node {
            // Only add led_to if previous wasn't already the goal
            if self.current_goal.as_ref() != Some(prev) {
                self.edges.push(GraphEdge::new(
                    prev.clone(),
                    node_id.clone(),
                    EdgeKind::LedTo,
                ));
                self.stats.edge_count += 1;
            }
        }

        self.push_node(node);
        node_id
    }

    /// Append an LLM response node — the agent's final answer for the turn.
    ///
    /// Created at turn end from the agent's closing message: the stop
    /// payload's response field (`last_assistant_message`,
    /// `prompt_response`, …) or, failing that, the last assistant entry of
    /// the session transcript. This is the durable record of what the agent
    /// *concluded*, complementing the tool-derived nodes that record what it
    /// *did*.
    ///
    /// Returns the new node's ID.
    pub fn append_llm_response(&mut self, text: &str, timestamp: i64) -> String {
        let summary = truncate_prompt(text, 200);
        let mut node = GraphNode::new(self.next_id(), NodeKind::LlmResponse, timestamp, &summary);

        let stored: String = text.chars().take(MAX_RESPONSE_TEXT_LEN).collect();
        node.detail = Some(serde_json::json!({
            "response_text": stored,
            "text_length": text.len(),
        }));

        // Mark as classified so the Phase 3 consolidator doesn't touch it
        node.classified = true;
        node.confidence = Some(1.0);

        let node_id = node.id.clone();

        // Edge: goal --led_to-→ response (the answer serves the prompt)
        if let Some(goal) = &self.current_goal {
            self.edges.push(GraphEdge::new(
                goal.clone(),
                node_id.clone(),
                EdgeKind::LedTo,
            ));
            self.stats.edge_count += 1;
        }

        // Also chain from the previous node for temporal ordering
        if let Some(prev) = &self.last_node {
            if self.current_goal.as_ref() != Some(prev) {
                self.edges.push(GraphEdge::new(
                    prev.clone(),
                    node_id.clone(),
                    EdgeKind::LedTo,
                ));
                self.stats.edge_count += 1;
            }
        }

        self.push_node(node);
        node_id
    }

    /// Append a human gate node (permission requested).
    ///
    /// Returns the new node's ID.
    pub fn append_human_gate(&mut self, reason: &str, timestamp: i64) -> String {
        let summary = truncate_prompt(reason, 200);
        let mut node = GraphNode::new(self.next_id(), NodeKind::HumanGate, timestamp, &summary);
        node.detail = Some(serde_json::json!({
            "reason": reason,
            "resolved": false,
        }));

        let node_id = node.id.clone();

        // Edge: last_node --blocked_by-→ gate
        if let Some(ref prev) = self.last_node {
            self.edges.push(GraphEdge::new(
                prev.clone(),
                node_id.clone(),
                EdgeKind::BlockedBy,
            ));
            self.stats.edge_count += 1;
        }

        self.pending_human_gate = Some(node_id.clone());

        self.push_node(node);
        node_id
    }

    /// Append a patch proposal node (change recorded).
    ///
    /// Returns the new node's ID.
    pub fn append_patch_proposal(
        &mut self,
        change_hash: &str,
        files: &[String],
        timestamp: i64,
    ) -> String {
        let file_summary = if files.is_empty() {
            String::new()
        } else if files.len() == 1 {
            files[0].clone()
        } else {
            format!("{} files", files.len())
        };

        let summary = if file_summary.is_empty() {
            format!("Change {}", short_hash(change_hash))
        } else {
            format!("Change {}: {}", short_hash(change_hash), file_summary)
        };

        let mut node = GraphNode::new(self.next_id(), NodeKind::PatchProposal, timestamp, &summary)
            .with_change_hash(change_hash);

        node.detail = Some(serde_json::json!({
            "change_hash": change_hash,
            "files": files,
        }));

        let node_id = node.id.clone();

        // Edge: each commitment since last patch --committed_via-→ this patch
        for commit_id in &self.commitments_since_last_patch {
            self.edges.push(GraphEdge::new(
                commit_id.clone(),
                node_id.clone(),
                EdgeKind::CommittedVia,
            ));
            self.stats.edge_count += 1;
        }

        // If no commitments, link from the goal
        if self.commitments_since_last_patch.is_empty() {
            if let Some(ref goal) = self.current_goal {
                self.edges.push(GraphEdge::new(
                    goal.clone(),
                    node_id.clone(),
                    EdgeKind::LedTo,
                ));
                self.stats.edge_count += 1;
            }
        }

        self.commitments_since_last_patch.clear();

        self.push_node(node);
        node_id
    }

    /// Append a raw node with full control over its fields.
    ///
    /// This method bypasses the usual edge inference logic and allows
    /// direct insertion of nodes with custom details. Used for ingesting
    /// external trace formats (e.g., Sherpa JSONL) that carry their own
    /// structured data.
    ///
    /// Returns the new node's ID.
    pub fn append_raw_node(
        &mut self,
        kind: NodeKind,
        timestamp: i64,
        summary: &str,
        detail: Option<serde_json::Value>,
    ) -> String {
        let node = GraphNode {
            id: self.next_id(),
            kind,
            timestamp,
            summary: summary.to_string(),
            detail,
            change_hash: None,
            tool_name: None,
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        };
        let node_id = node.id.clone();
        self.push_node(node);
        node_id
    }

    /// Append a generic todo snapshot and link it to the active turn goal.
    pub fn append_todo_snapshot(
        &mut self,
        todo: &atomic_core::change::session::SessionTodo,
        timestamp: i64,
    ) -> String {
        let node = GraphNode {
            id: self.next_id(),
            kind: NodeKind::Todo,
            timestamp,
            summary: todo.content.clone(),
            detail: Some(serde_json::json!({
                "todo_id": todo.id,
                "content": todo.content,
                "status": todo.status,
                "priority": todo.priority,
                "record_type": "todo",
            })),
            change_hash: None,
            tool_name: None,
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        };
        let node_id = node.id.clone();
        if let Some(goal) = self.current_goal.clone() {
            self.edges
                .push(GraphEdge::new(goal, &node_id, EdgeKind::LedTo));
            self.stats.edge_count += 1;
        }
        self.push_node(node);
        node_id
    }

    /// Mark a human gate as resolved.
    ///
    /// Updates the gate node's detail and clears the pending gate state.
    /// The next node appended after this will get a `ResumedAfter` edge
    /// from the goal (if any), since the gate is cleared by `append_goal`.
    pub fn resolve_human_gate(&mut self, gate_id: &str) {
        // Update the node's detail to mark it resolved
        if let Some(node) = self.nodes.iter_mut().find(|n| n.id == gate_id) {
            if let Some(ref mut detail) = node.detail {
                if let Some(obj) = detail.as_object_mut() {
                    obj.insert("resolved".into(), serde_json::Value::Bool(true));
                }
            }
        }

        // Clear pending gate if it matches
        if self.pending_human_gate.as_deref() == Some(gate_id) {
            self.pending_human_gate = None;
        }
    }

    // =========================================================================
    // Edge inference
    // =========================================================================

    /// Infer and add edges when a new tool-derived node is appended.
    pub(crate) fn infer_edges(&mut self, node_id: &str, kind: NodeKind) {
        match kind {
            NodeKind::Exploration => {
                // Exploration serves the current goal
                if let Some(ref goal) = self.current_goal {
                    self.edges.push(GraphEdge::new(
                        goal.clone(),
                        node_id.to_string(),
                        EdgeKind::LedTo,
                    ));
                    self.stats.edge_count += 1;
                }
                self.pending_explorations.push(node_id.to_string());
            }

            NodeKind::Commitment => {
                if self.pending_explorations.is_empty() {
                    // No explorations preceded this — link from goal
                    if let Some(ref goal) = self.current_goal {
                        self.edges.push(GraphEdge::new(
                            goal.clone(),
                            node_id.to_string(),
                            EdgeKind::LedTo,
                        ));
                        self.stats.edge_count += 1;
                    }
                } else {
                    // Link from each pending exploration
                    for exp_id in &self.pending_explorations {
                        self.edges.push(GraphEdge::new(
                            exp_id.clone(),
                            node_id.to_string(),
                            EdgeKind::ExploredVia,
                        ));
                        self.stats.edge_count += 1;
                    }
                    self.pending_explorations.clear();
                }

                self.last_commitment = Some(node_id.to_string());
                self.commitments_since_last_patch.push(node_id.to_string());
            }

            NodeKind::Verification => {
                // Verification validates the most recent commitment
                if let Some(ref commit) = self.last_commitment {
                    self.edges.push(GraphEdge::new(
                        commit.clone(),
                        node_id.to_string(),
                        EdgeKind::VerifiedBy,
                    ));
                    self.stats.edge_count += 1;
                } else if let Some(ref goal) = self.current_goal {
                    // No commitment to verify — link from goal
                    self.edges.push(GraphEdge::new(
                        goal.clone(),
                        node_id.to_string(),
                        EdgeKind::LedTo,
                    ));
                    self.stats.edge_count += 1;
                }
            }

            NodeKind::Execution => {
                // Execution serves the current goal
                if let Some(ref goal) = self.current_goal {
                    self.edges.push(GraphEdge::new(
                        goal.clone(),
                        node_id.to_string(),
                        EdgeKind::LedTo,
                    ));
                    self.stats.edge_count += 1;
                }
            }

            NodeKind::Error => {
                // Error caused by whatever preceded it
                if let Some(ref prev) = self.last_node {
                    self.edges.push(GraphEdge::new(
                        prev.clone(),
                        node_id.to_string(),
                        EdgeKind::FailedWith,
                    ));
                    self.stats.edge_count += 1;
                }
            }

            // Goal, HumanGate, PatchProposal handle their own edges in
            // their append_* methods. Decision nodes are created by the
            // Phase 3 classifier, not by append_tool_call.
            _ => {}
        }
    }
}
