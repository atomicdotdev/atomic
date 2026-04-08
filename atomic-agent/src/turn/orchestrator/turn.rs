//! Turn processing for the turn orchestrator.
//!
//! Contains handlers for TurnStart, TurnEnd, and ToolUse events.

use std::path::Path;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::record::{record_turn, TurnRecordOptions};
use crate::turn::phase::{self, Action, Event, TransitionContext};

use super::{DispatchResult, TurnOrchestrator};

impl TurnOrchestrator {
    /// Handle a TurnStart event (UserPromptSubmit).
    ///
    /// Begins file watching and transitions the session to Active.
    pub(super) async fn handle_turn_start(
        &mut self,
        event: TurnEvent,
    ) -> AgentResult<DispatchResult> {
        let session_id = &event.session_id;

        let mut session = self.load_or_create_session(session_id, &event)?;

        // Store the prompt
        if let Some(ref prompt) = event.prompt {
            session.set_first_prompt(prompt);
        }

        // Update transcript path
        if let Some(ref path) = event.transcript_path {
            session.set_transcript_path(path);
        }

        // Extract model/provider from raw_json if present.
        // OpenCode sends: {"model": "claude-opus-4-5", "provider": "anthropic", ...}
        // This is the most reliable source of model info — it comes from the
        // chat.message hook which fires at the start of every turn.
        if let Some(ref raw) = event.raw_json {
            if let Some(model) = raw.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    session.model = model.to_string();
                }
            }
            if let Some(provider) = raw.get("provider").and_then(|v| v.as_str()) {
                if !provider.is_empty() {
                    session.agent_vendor = provider.to_string();
                }
            }
        }

        // Begin file watching for this turn
        if let Err(e) = self.watcher.begin_turn(session_id).await {
            log::warn!(
                "Failed to begin file watching for session {}: {}",
                session_id,
                e
            );
            // Continue anyway — we'll just miss file changes
        }

        // Mark the turn as started in the session
        session.begin_turn();

        // State machine transition
        let result = phase::transition(
            session.phase,
            Event::TurnStart,
            TransitionContext::default(),
        );
        let remaining = phase::apply_common_actions(&mut session, &result);

        // Handle strategy-specific actions
        let mut dispatch = DispatchResult::new(session_id, session.phase);
        for action in &remaining {
            if let Action::WarnStaleSession = action {
                dispatch = dispatch.with_warning(format!(
                    "Turn started while session {} was already active (Ctrl-C recovery)",
                    session_id
                ));
            }
        }

        // Provenance: append a goal node from the user's prompt.
        // Best-effort — failures are logged but never block the session.
        if let Some(ref prompt) = event.prompt {
            if !prompt.is_empty() {
                if let Some(mut acc) = self.load_accumulator(session_id) {
                    acc.append_goal(prompt, event.timestamp.timestamp());
                    self.save_accumulator(session_id, &acc);
                }
            }
        }

        self.session_store.save(&session)?;

        Ok(dispatch)
    }

    /// Handle a TurnEnd event (Stop).
    ///
    /// Records an Atomic change for the turn (status → add → record), then
    /// transitions the session back to Idle.
    ///
    /// The recording workflow lets the repository figure out what changed:
    /// 1. `repo.status()` — find modified, deleted, and untracked files
    /// 2. `repo.add()` — track any new files the agent created
    /// 3. `repo.record(all: true)` — record everything that's dirty
    ///
    /// This avoids the cross-process watcher state problem: each hook
    /// invocation is a separate process, so we can't carry in-memory
    /// snapshots between TurnStart and TurnEnd. Instead, we ask the
    /// repository what changed since the last recorded state.
    pub(super) async fn handle_turn_end(
        &mut self,
        event: TurnEvent,
    ) -> AgentResult<DispatchResult> {
        let session_id = &event.session_id;

        let mut session = self.load_or_create_session(session_id, &event)?;

        // Extract model/provider from the TurnEnd event's raw_json.
        // OpenCode sends model and provider in every stop payload.
        // This is the last chance to capture the info before recording,
        // in case TurnStart didn't have it (e.g., session was created
        // outside the plugin, or the chat.message hook didn't fire).
        if let Some(ref raw) = event.raw_json {
            if let Some(model) = raw.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    session.model = model.to_string();
                }
            }
            if let Some(provider) = raw.get("provider").and_then(|v| v.as_str()) {
                if !provider.is_empty() {
                    session.agent_vendor = provider.to_string();
                }
            }
        }

        // Release the watcher if it was active (best-effort, ignore errors)
        if self.watcher.is_active() {
            let _ = self.watcher.cancel_turn().await;
        }

        // Compute turn metadata.
        // Prefer the plugin-provided duration (chat.message → session.idle wall-clock)
        // over the Rust-side computation (user-prompt CLI → stop CLI, which is ~0ms
        // because the plugin sends both in rapid succession at idle time).
        let plugin_duration_ms = event
            .raw_json
            .as_ref()
            .and_then(|r| r.get("turn_duration_ms"))
            .and_then(|v| v.as_u64());
        let turn_duration_ms =
            plugin_duration_ms.unwrap_or_else(|| session.current_turn_duration_ms().unwrap_or(0));
        let turn_number = session.end_turn(); // increments turn_count, returns new count

        // State machine transition — always say files MAY have changed.
        // The actual check happens inside record_turn() which returns
        // EmptyTurn if nothing changed.
        let result = phase::transition(
            session.phase,
            Event::TurnEnd,
            TransitionContext {
                has_files_changed: true, // optimistic — record_turn will verify
            },
        );
        let remaining = phase::apply_common_actions(&mut session, &result);

        // Execute strategy-specific actions
        let mut dispatch = DispatchResult::new(session_id, session.phase);

        for action in &remaining {
            match action {
                Action::RecordTurn | Action::RecordIfChanged => {
                    // Get the prompt for this turn's change message.
                    // Priority: event.prompt (from TurnEnd, rare)
                    //         > session.current_prompt (set on each TurnStart)
                    //         > session.first_prompt (fallback for legacy/missing)
                    let prompt = event
                        .prompt
                        .clone()
                        .or_else(|| session.current_prompt.clone())
                        .or_else(|| session.first_prompt.clone());

                    let record_options = TurnRecordOptions {
                        session: &session,
                        event: &event,
                        turn_number,
                        turn_duration_ms,
                        prompt,
                    };

                    match record_turn(&self.repo_root, &record_options) {
                        Ok(outcome) => {
                            // Track the recorded files in the session
                            let recorded_files: Vec<String> = outcome.recorded_file_list().to_vec();
                            session.add_files_touched(&recorded_files);

                            // Clear current_prompt so the next turn doesn't
                            // reuse this turn's prompt as the change message.
                            session.clear_current_prompt();

                            // Sherpa: ingest the JSONL trace file into the provenance accumulator
                            // so the resulting ProvenanceGraph has rich node data (file attribution,
                            // bash commands, todo structure) instead of just the thin Goal node.
                            if let Some(trace_path) = event
                                .raw_json
                                .as_ref()
                                .and_then(|r| r.get("trace_file"))
                                .and_then(|v| v.as_str())
                            {
                                self.ingest_sherpa_trace(session_id, Path::new(trace_path));
                            }

                            // Provenance: inject reasoning blocks as Decision nodes,
                            // then append a patch proposal node and save the graph.
                            self.inject_reasoning_nodes(session_id, &event);
                            self.save_turn_provenance(session_id, &session, &outcome, &event);

                            log::info!(
                                "Recorded turn {} for session {}: {}",
                                turn_number,
                                session_id,
                                outcome
                            );
                            dispatch = dispatch.with_change(outcome);
                        }
                        Err(AgentError::EmptyTurn { .. }) => {
                            // No files changed — this is normal (e.g., agent
                            // only read files, didn't modify anything)
                            log::info!(
                                "Turn {} for session {} had no changes — skipping record",
                                turn_number,
                                session_id
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "Failed to record turn {} for session {}: {}",
                                turn_number,
                                session_id,
                                e
                            );
                            dispatch = dispatch.with_warning(format!(
                                "Failed to record turn {}: {}",
                                turn_number, e
                            ));
                        }
                    }
                }

                Action::DiscardIfNoFiles => {
                    log::info!(
                        "Session {} ended with no files touched — discarding",
                        session_id
                    );
                }

                Action::WarnStaleSession => {
                    dispatch =
                        dispatch.with_warning(format!("Stale session warning for {}", session_id));
                }

                _ => {
                    // UpdateInteraction, ClearEndedAt handled by apply_common_actions
                }
            }
        }

        self.session_store.save(&session)?;

        Ok(dispatch)
    }

    /// Handle a PreToolUse or PostToolUse event.
    ///
    /// Currently tracks tool usage for informational purposes. Future
    /// enhancements may create sub-turn recordings for sub-agent (Task) tools.
    pub(super) async fn handle_tool_use(
        &mut self,
        event: TurnEvent,
    ) -> AgentResult<DispatchResult> {
        let session_id = &event.session_id;

        let session = match self.session_store.load(session_id)? {
            Some(s) => s,
            None => {
                log::debug!("ToolUse for unknown session {} — ignoring", session_id);
                return Ok(DispatchResult::new(
                    session_id,
                    crate::turn::phase::Phase::Idle,
                ));
            }
        };

        // Log tool usage
        if let Some(ref tool_name) = event.tool_name {
            log::info!(
                "Session {} tool use: {} ({})",
                session_id,
                tool_name,
                event.event_type,
            );
        }

        // Provenance: append tool call nodes on PostToolUse.
        //
        // PreToolUse doesn't have output or duration yet, so we only
        // record on PostToolUse where the full picture is available.
        // The classifier uses tool name + input + output to determine
        // the node kind (Exploration, Commitment, Verification, etc.).
        if event.event_type == HookType::PostToolUse {
            if let Some(mut acc) = self.load_accumulator(session_id) {
                let tool_name = event.tool_name.as_deref().unwrap_or("unknown");
                let tool_call_id = event.tool_use_id.as_deref();

                // Extract tool_input, tool_output, status, duration from raw_json.
                //
                // The enriched OpenCode plugin sends top-level fields alongside
                // tool_input: filediff, diagnostics, title, file_path, exit_code.
                // We merge these INTO tool_input so the accumulator's classify
                // and detail-building functions can find them without changing
                // their signature.
                let raw = event.raw_json.as_ref();
                let tool_output = raw.and_then(|r| r.get("tool_output").and_then(|v| v.as_str()));
                let status = raw.and_then(|r| r.get("status").and_then(|v| v.as_str()));
                let duration_ms = raw.and_then(|r| r.get("duration").and_then(|v| v.as_u64()));

                // Build a merged tool_input that includes both the original
                // tool_input fields AND the top-level enriched fields.
                let merged_input: Option<serde_json::Value> = raw.map(|r| {
                    let mut merged = r
                        .get("tool_input")
                        .and_then(|v| v.as_object().cloned())
                        .unwrap_or_default();

                    // Merge enriched top-level fields into tool_input
                    for key in &[
                        "filediff",
                        "diagnostics",
                        "title",
                        "file_path",
                        "exit_code",
                        "diff",
                    ] {
                        if let Some(val) = r.get(*key) {
                            merged.insert(key.to_string(), val.clone());
                        }
                    }

                    serde_json::Value::Object(merged)
                });

                acc.append_tool_call(
                    tool_name,
                    tool_call_id,
                    merged_input.as_ref(),
                    tool_output,
                    status,
                    duration_ms,
                    event.timestamp.timestamp(),
                );

                self.save_accumulator(session_id, &acc);
            }
        }

        Ok(DispatchResult::new(session_id, session.phase))
    }
}
