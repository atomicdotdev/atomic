//! Provenance graph helpers and attestation creation for the turn orchestrator.
//!
//! Contains owner-journal migration, event normalization, deterministic
//! checkpoint finalization, transcript recovery, and attestation helpers.
//! Mutable provenance is committed through the repository owner. Legacy
//! `graph.json` is read once for pending-delta migration and then removed.

use std::path::{Path, PathBuf};

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, ProvenanceJournalEnvelope, ProvenanceJournalEvent, TurnEvent};
use crate::provenance::accumulator::ProvenanceAccumulator;
use crate::record::TurnRecordOutcome;
use crate::transcript;
use crate::turn::session::AgentSession;
use atomic_core::change::session::SessionTodo;

use super::{truncate_prompt, TurnOrchestrator};

struct JournalDraft {
    event_id: String,
    timestamp_ms: i64,
    causal_parent_ids: Vec<String>,
    event: ProvenanceJournalEvent,
}

impl TurnOrchestrator {
    fn migrate_legacy_accumulator(
        &self,
        session_id: &str,
        turn_number: u32,
        timestamp_ms: i64,
    ) -> AgentResult<()> {
        let session_dir = self.session_graph_dir(session_id);
        let graph_path = ProvenanceAccumulator::graph_path(&session_dir);
        if !graph_path.exists() {
            // A stale lock has no authority without its graph payload.
            let _ = std::fs::remove_file(session_dir.join("graph.lock"));
            return Ok(());
        }
        let accumulator = ProvenanceAccumulator::load_or_create(&session_dir, session_id)?;
        if let Some(event) = accumulator.pending_graph_delta() {
            if self.journal_sink.is_none() {
                return Err(AgentError::ProvenanceJournalFailed {
                    session_id: session_id.to_string(),
                    reason: "legacy graph.json requires an owner-backed migration sink".to_string(),
                });
            }
            self.commit_journal_drafts(
                session_id,
                turn_number,
                vec![JournalDraft {
                    event_id: stable_journal_id(session_id, turn_number, "legacy-graph-import"),
                    timestamp_ms,
                    causal_parent_ids: Vec::new(),
                    event,
                }],
            )?;
        }
        // Deletion is strictly after parse and committed acknowledgement (when
        // a pending delta existed). Immutable content-addressed history is not
        // stored here and remains untouched.
        std::fs::remove_file(&graph_path).map_err(|error| AgentError::SessionSaveFailed {
            session_id: session_id.to_string(),
            reason: format!("failed to remove migrated graph.json: {error}"),
        })?;
        let _ = std::fs::remove_file(session_dir.join("graph.lock"));
        Ok(())
    }

    pub(super) fn resume_journal_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        now: i64,
    ) -> AgentResult<()> {
        let Some(sink) = &self.journal_sink else {
            return Ok(());
        };
        sink.resume_turn(session_id, turn_number, now)
            .map(|_| ())
            .map_err(|reason| AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason,
            })
    }

    pub(super) fn stop_journal_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        cause: super::JournalStopCause,
        resumable: bool,
        observed_at: i64,
    ) -> AgentResult<()> {
        let Some(sink) = &self.journal_sink else {
            return Ok(());
        };
        sink.stop_turn(session_id, turn_number, cause, resumable, observed_at)
            .map(|_| ())
            .map_err(|reason| AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason,
            })
    }

    fn commit_journal_drafts(
        &self,
        session_id: &str,
        turn_number: u32,
        drafts: Vec<JournalDraft>,
    ) -> AgentResult<()> {
        let Some(sink) = &self.journal_sink else {
            return Ok(());
        };
        if drafts.is_empty() {
            return Ok(());
        }
        let now = drafts
            .iter()
            .map(|draft| draft.timestamp_ms.div_euclid(1000))
            .max()
            .unwrap_or(0);
        let reservation = sink
            .reserve_turn(session_id, turn_number, now)
            .map_err(|reason| AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason,
            })?;
        let envelopes: Vec<_> = drafts
            .into_iter()
            .map(|draft| {
                ProvenanceJournalEnvelope::new(
                    draft.event_id,
                    session_id,
                    turn_number,
                    reservation.generation,
                    draft.timestamp_ms,
                    draft.event,
                )
                .with_causal_parents(draft.causal_parent_ids)
            })
            .collect();
        let expected_ids: Vec<_> = envelopes
            .iter()
            .map(|envelope| envelope.event_id.clone())
            .collect();
        let acknowledgements = sink.append(reservation, envelopes, now).map_err(|reason| {
            AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason,
            }
        })?;
        let acknowledged_ids: std::collections::HashSet<_> = acknowledgements
            .into_iter()
            .map(|ack| ack.event_id)
            .collect();
        if expected_ids
            .iter()
            .any(|event_id| !acknowledged_ids.contains(event_id))
        {
            return Err(AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason: "owner omitted a committed event acknowledgement".to_string(),
            });
        }
        Ok(())
    }

    pub(super) fn commit_hook_event(&self, event: &TurnEvent, turn_number: u32) -> AgentResult<()> {
        self.migrate_legacy_accumulator(
            &event.session_id,
            turn_number,
            event.timestamp.timestamp_millis(),
        )?;
        let discriminator = match event.event_type {
            HookType::TurnStart => "goal".to_string(),
            HookType::PreToolUse => format!(
                "tool-before:{}",
                event
                    .tool_use_id
                    .clone()
                    .unwrap_or_else(|| anonymous_tool_key(event))
            ),
            HookType::PostToolUse => format!(
                "tool-after:{}",
                event
                    .tool_use_id
                    .clone()
                    .unwrap_or_else(|| anonymous_tool_key(event))
            ),
            HookType::TurnEnd => "turn-end".to_string(),
            HookType::SessionStart => "session-start".to_string(),
            HookType::SessionEnd => "session-end".to_string(),
        };
        let event_id = stable_journal_id(&event.session_id, turn_number, &discriminator);
        let envelope = ProvenanceJournalEnvelope::from_turn_event(&event_id, turn_number, 1, event);
        self.commit_journal_drafts(
            &event.session_id,
            turn_number,
            vec![JournalDraft {
                event_id,
                timestamp_ms: envelope.timestamp_ms,
                causal_parent_ids: Vec::new(),
                event: envelope.event,
            }],
        )
    }

    pub(super) fn commit_turn_completion_events(
        &self,
        session: &AgentSession,
        event: &TurnEvent,
        turn_number: u32,
    ) -> AgentResult<()> {
        self.migrate_legacy_accumulator(
            &event.session_id,
            turn_number,
            event.timestamp.timestamp_millis(),
        )?;
        let mut drafts = Vec::new();
        let mut previous = None::<String>;
        let mut terminal_parents = Vec::new();
        for (index, (text, duration_ms, signature)) in
            reasoning_blocks(event).into_iter().enumerate()
        {
            let event_id = stable_journal_id(
                &event.session_id,
                turn_number,
                &format!("reasoning:{index}"),
            );
            drafts.push(JournalDraft {
                event_id: event_id.clone(),
                timestamp_ms: event.timestamp.timestamp_millis(),
                causal_parent_ids: previous.iter().cloned().collect(),
                event: ProvenanceJournalEvent::Reasoning {
                    text,
                    duration_ms,
                    signature,
                },
            });
            previous = Some(event_id);
        }

        if let Some(last_reasoning) = previous.clone() {
            terminal_parents.push(last_reasoning);
        }
        for todo in extract_turn_todos(event, &event.session_id, turn_number.saturating_sub(1)) {
            let event_id =
                stable_journal_id(&event.session_id, turn_number, &format!("todo:{}", todo.id));
            terminal_parents.push(event_id.clone());
            drafts.push(JournalDraft {
                event_id,
                timestamp_ms: event.timestamp.timestamp_millis(),
                causal_parent_ids: Vec::new(),
                event: ProvenanceJournalEvent::Todo { todo },
            });
        }

        if let Some(text) = response_text(session, event) {
            let event_id = stable_journal_id(&event.session_id, turn_number, "response");
            drafts.push(JournalDraft {
                event_id: event_id.clone(),
                timestamp_ms: event.timestamp.timestamp_millis(),
                causal_parent_ids: previous.iter().cloned().collect(),
                event: ProvenanceJournalEvent::Response { text },
            });
            terminal_parents.push(event_id);
        }

        let terminal_id = stable_journal_id(&event.session_id, turn_number, "turn-end");
        drafts.push(JournalDraft {
            event_id: terminal_id,
            timestamp_ms: event.timestamp.timestamp_millis(),
            causal_parent_ids: terminal_parents,
            event: ProvenanceJournalEvent::Terminal {
                hook_type: HookType::TurnEnd,
                reason: event
                    .raw_json
                    .as_ref()
                    .and_then(|raw| raw.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
                payload: event.raw_json.clone(),
            },
        });
        self.commit_journal_drafts(&event.session_id, turn_number, drafts)
    }

    fn commit_recorded_turn_events(
        &self,
        session_id: &str,
        turn_number: u32,
        event: &TurnEvent,
        outcome: &TurnRecordOutcome,
    ) -> AgentResult<()> {
        use atomic_core::types::Base32;

        let mut drafts = Vec::new();
        let change_hash = outcome.hash.to_base32();
        drafts.push(JournalDraft {
            event_id: stable_journal_id(session_id, turn_number, &format!("patch:{change_hash}")),
            timestamp_ms: event.timestamp.timestamp_millis(),
            causal_parent_ids: Vec::new(),
            event: ProvenanceJournalEvent::PatchProposal {
                change_hash,
                files: outcome.recorded_file_list().to_vec(),
            },
        });
        self.commit_journal_drafts(session_id, turn_number, drafts)
    }

    /// Get the session directory used by session-adjacent migration/transcripts.
    pub(crate) fn session_graph_dir(&self, session_id: &str) -> PathBuf {
        self.session_store.sessions_dir().join(session_id)
    }

    /// OpenCode: recover transcript, reasoning, and response from
    /// OpenCode's local SQLite store and fold them into the session and
    /// the stop payload before recording.
    ///
    /// OpenCode writes no transcript file, so its sessions never carry a
    /// `transcript_path`, and thin plugin versions forward only
    /// session/model metadata. Without this step an OpenCode turn could
    /// never carry `agent_turn` data, reasoning, or an `llm_response`
    /// node. The recovered data lands exactly where the existing pipeline
    /// reads it:
    ///
    /// - the transcript is synthesized into the session directory and set
    ///   as `session.transcript_path` (consumed by
    ///   `build_unhashed_turn_data` via the `opencode` condense format);
    /// - the turn's reasoning/response/token/cost fields are injected into
    ///   `event.raw_json` only when the plugin didn't send them; the owner
    ///   journal then commits the enriched terminal envelopes.
    ///
    pub(crate) fn enrich_opencode_turn(
        &self,
        session: &mut AgentSession,
        event: &mut TurnEvent,
    ) -> AgentResult<()> {
        if session.agent_name != "opencode" {
            return Ok(());
        }

        let Some(data) = transcript::opencode::read_turn(&session.session_id, &self.repo_root)
        else {
            return Ok(());
        };

        // 1. Transcript file → transcript_path (unblocks agent_turn).
        // Rewritten on every turn (whole-session record); a plugin-supplied
        // transcript path is never clobbered.
        if !data.transcript_jsonl.is_empty() {
            let dir = self.session_graph_dir(&session.session_id);
            let synthesized = dir.join("opencode-transcript.jsonl");
            let ours = session.transcript_path.is_none()
                || session.transcript_path.as_deref() == Some(synthesized.as_path());
            if ours {
                let _ = std::fs::create_dir_all(&dir);
                match std::fs::write(&synthesized, &data.transcript_jsonl) {
                    Ok(()) => session.set_transcript_path(&synthesized),
                    Err(e) => log::warn!(
                        "Failed to write opencode transcript for session {}: {}",
                        session.session_id,
                        e
                    ),
                }
            }
        }

        // 2. Stop-payload fields the plugin didn't send.
        let Some(raw) = event.raw_json.as_mut() else {
            return Ok(());
        };
        let Some(obj) = raw.as_object_mut() else {
            return Ok(());
        };

        if !obj.contains_key("reasoning_blocks")
            && !obj.contains_key("reasoning_text")
            && !data.reasoning_blocks.is_empty()
        {
            let blocks: Vec<serde_json::Value> = data
                .reasoning_blocks
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "text": b.text,
                        "duration_ms": b.duration_ms,
                    })
                })
                .collect();
            obj.insert(
                "reasoning_blocks".to_string(),
                serde_json::Value::Array(blocks),
            );
        }

        if let Some(response) = &data.response {
            if !obj.contains_key("last_assistant_message") && !obj.contains_key("response") {
                obj.insert("response".to_string(), serde_json::json!(response));
            }
        }

        let have_tokens = obj.contains_key("input_tokens") || obj.contains_key("output_tokens");
        if !have_tokens
            && (data.input_tokens > 0 || data.output_tokens > 0 || data.reasoning_tokens > 0)
        {
            obj.insert(
                "input_tokens".to_string(),
                serde_json::json!(data.input_tokens),
            );
            obj.insert(
                "output_tokens".to_string(),
                serde_json::json!(data.output_tokens),
            );
            obj.insert(
                "reasoning_tokens".to_string(),
                serde_json::json!(data.reasoning_tokens),
            );
            obj.insert(
                "cache_read_tokens".to_string(),
                serde_json::json!(data.cache_read_tokens),
            );
            obj.insert(
                "cache_write_tokens".to_string(),
                serde_json::json!(data.cache_write_tokens),
            );
        }

        if !obj.contains_key("cost_usd") && data.cost_usd > 0.0 {
            obj.insert("cost_usd".to_string(), serde_json::json!(data.cost_usd));
        }
        if !obj.contains_key("finish_reason") {
            if let Some(reason) = &data.finish_reason {
                obj.insert("finish_reason".to_string(), serde_json::json!(reason));
            }
        }
        if !obj.contains_key("step_count") && data.step_count > 0 {
            obj.insert("step_count".to_string(), serde_json::json!(data.step_count));
        }

        let drafts = data
            .tool_parts
            .iter()
            .map(|tool| JournalDraft {
                event_id: stable_journal_id(
                    &session.session_id,
                    session.turn_count.saturating_add(1),
                    &format!("opencode-tool-enrichment:{}", tool.call_id),
                ),
                timestamp_ms: event.timestamp.timestamp_millis(),
                causal_parent_ids: Vec::new(),
                event: ProvenanceJournalEvent::ToolEnrichment {
                    tool_call_id: tool.call_id.clone(),
                    tool_name: tool.tool.clone(),
                    input: tool.input.clone(),
                    output: tool.output.clone(),
                    status: tool.status.clone(),
                },
            })
            .collect();
        self.commit_journal_drafts(
            &session.session_id,
            session.turn_count.saturating_add(1),
            drafts,
        )?;

        log::info!(
            "Recovered opencode turn data from local store for session {} \
             ({} reasoning block{}, response: {}, {} step{})",
            session.session_id,
            data.reasoning_blocks.len(),
            if data.reasoning_blocks.len() == 1 {
                ""
            } else {
                "s"
            },
            data.response.is_some(),
            data.step_count,
            if data.step_count == 1 { "" } else { "s" },
        );
        Ok(())
    }

    /// Read a Sherpa JSONL trace file and create provenance nodes for
    /// every record, preserving the full agent-trace + Sherpa extension data.
    ///
    /// Returns `true` if at least one record was successfully ingested.
    pub(crate) fn ingest_sherpa_trace(
        &self,
        session_id: &str,
        turn_number: u32,
        trace_path: &Path,
    ) -> AgentResult<bool> {
        use crate::provenance::types::{GraphNode, NodeKind};

        let content = match std::fs::read_to_string(trace_path) {
            Ok(content) => content,
            Err(error) => {
                log::warn!(
                    "sherpa trace: failed to read {}: {}",
                    trace_path.display(),
                    error
                );
                return Ok(false);
            }
        };
        let mut drafts = Vec::new();
        for (line_no, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record: serde_json::Value = match serde_json::from_str(line) {
                Ok(record) => record,
                Err(error) => {
                    log::warn!("sherpa trace: line {} parse error: {}", line_no + 1, error);
                    continue;
                }
            };
            let dev_atomic = &record["metadata"]["dev.atomic"];
            let record_type = dev_atomic["record_type"].as_str().unwrap_or("unknown");
            let timestamp = record["timestamp"]
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp_millis())
                .unwrap_or(0);
            let (kind, summary) = match record_type {
                "intent" => (
                    NodeKind::Goal,
                    dev_atomic["intent_title"]
                        .as_str()
                        .unwrap_or("intent")
                        .to_string(),
                ),
                "commitment" => {
                    let file = record["files"]
                        .as_array()
                        .and_then(|files| files.first())
                        .and_then(|file| file["path"].as_str())
                        .unwrap_or("");
                    (NodeKind::Commitment, format!("wrote {file}"))
                }
                "execution" => (
                    NodeKind::Execution,
                    dev_atomic["command"]
                        .as_str()
                        .unwrap_or("command")
                        .to_string(),
                ),
                "todo" => (
                    NodeKind::Todo,
                    format!(
                        "[{}] {}",
                        dev_atomic["todo_id"].as_str().unwrap_or(""),
                        dev_atomic["content"].as_str().unwrap_or("")
                    ),
                ),
                "todo_status" => (
                    NodeKind::TodoStatusChange,
                    format!(
                        "{}: {} → {}",
                        dev_atomic["todo_id"].as_str().unwrap_or(""),
                        dev_atomic["from_status"].as_str().unwrap_or(""),
                        dev_atomic["to_status"].as_str().unwrap_or("")
                    ),
                ),
                "phase_transition" => (
                    NodeKind::PhaseTransition,
                    format!(
                        "{} → {}",
                        dev_atomic["from_phase"].as_str().unwrap_or(""),
                        dev_atomic["to_phase"].as_str().unwrap_or("")
                    ),
                ),
                "lesson" => (
                    NodeKind::Lesson,
                    dev_atomic["label"].as_str().unwrap_or("lesson").to_string(),
                ),
                "llm_response" => (
                    NodeKind::LlmResponse,
                    truncate_prompt(dev_atomic["reply"].as_str().unwrap_or("llm response"), 200),
                ),
                "verification" => (
                    NodeKind::Verification,
                    dev_atomic["summary"]
                        .as_str()
                        .unwrap_or("verification")
                        .to_string(),
                ),
                "human_gate" => (
                    NodeKind::HumanGateResolution,
                    format!(
                        "resolution: {}",
                        dev_atomic["resolution"].as_str().unwrap_or("")
                    ),
                ),
                _ => continue,
            };
            let discriminator = format!("sherpa:{}", blake3::hash(line.as_bytes()).to_hex());
            let event_id = stable_journal_id(session_id, turn_number, &discriminator);
            let node =
                GraphNode::new(&event_id, kind, timestamp, summary).with_detail(dev_atomic.clone());
            drafts.push(JournalDraft {
                event_id,
                timestamp_ms: timestamp,
                causal_parent_ids: Vec::new(),
                event: ProvenanceJournalEvent::GraphDelta {
                    nodes: vec![node],
                    edges: Vec::new(),
                },
            });
        }
        let ingested = drafts.len();
        self.commit_journal_drafts(session_id, turn_number, drafts)?;
        Ok(ingested > 0)
    }

    /// Finalize a recorded turn through the persisted checkpoint state machine.
    pub(crate) fn save_turn_provenance(
        &self,
        session_id: &str,
        session: &AgentSession,
        outcome: &TurnRecordOutcome,
        event: &TurnEvent,
    ) -> AgentResult<()> {
        self.commit_recorded_turn_events(session_id, session.turn_count.max(1), event, outcome)?;
        self.checkpoint_turn_provenance(session_id, session, &[outcome.hash], event)
    }

    pub(super) fn checkpoint_turn_provenance(
        &self,
        session_id: &str,
        session: &AgentSession,
        change_hashes: &[atomic_core::types::Hash],
        event: &TurnEvent,
    ) -> AgentResult<()> {
        use atomic_core::change::session::SessionTurn;
        use atomic_core::types::{Base32, Hash};

        let Some(sink) = self.journal_sink.as_ref() else {
            log::debug!(
                "Skipping provenance finalization for {} without an owner journal sink",
                session_id
            );
            return Ok(());
        };

        // A failed read must not masquerade as a new session, which would
        // bind a checkpoint with a missing predecessor. Release this handle
        // before replaying the journal and opening for publication.
        let ledger = {
            let repository = atomic_repository::Repository::open_readonly_wait(
                &self.repo_root,
                std::time::Duration::from_secs(10),
            )
            .map_err(|error| AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason: error.to_string(),
            })?;
            repository.get_session_ledger(session_id).map_err(|error| {
                AgentError::ProvenanceJournalFailed {
                    session_id: session_id.to_string(),
                    reason: error.to_string(),
                }
            })?
        };
        let (previous_provenance, ledger_turn_number) = ledger
            .map(|(_, turns)| {
                (
                    turns.last().map(|turn| turn.provenance_hash),
                    turns.len() as u32,
                )
            })
            .unwrap_or((None, 0));
        let source = super::JournalCheckpointSource {
            agent_name: session.agent_name.clone(),
            agent_display_name: session.agent_display_name.clone(),
            agent_vendor: session.agent_vendor.clone(),
            change_hashes: change_hashes.to_vec(),
            previous_provenance,
            plan_id: session
                .managed_run
                .as_ref()
                .and_then(|run| run.work_item_id.clone()),
            ledger_turn_number,
        };
        let reservation = sink
            .reserve_turn(
                session_id,
                session.turn_count.max(1),
                event.timestamp.timestamp(),
            )
            .map_err(|reason| AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason,
            })?;
        let mut checkpoint = sink
            .prepare_checkpoint(reservation, source, event.timestamp.timestamp())
            .map_err(|reason| AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason,
            })?;

        let changes_dir = atomic_repository::Repository::canonical_dot_dir(&self.repo_root)
            .map_err(|error| AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason: error.to_string(),
            })?
            .join("changes");
        let change_store = atomic_repository::ChangeStore::new(
            changes_dir,
            atomic_repository::DEFAULT_CACHE_CAPACITY,
        )
        .map_err(|error| AgentError::ProvenanceJournalFailed {
            session_id: session_id.to_string(),
            reason: error.to_string(),
        })?;

        let (graph, _provenance_hash, session_turn) =
            match (checkpoint.provenance_hash, checkpoint.session_turn.clone()) {
                (Some(hash), Some(turn)) => {
                    let graph = change_store.load_provenance_graph(&hash).map_err(|error| {
                        AgentError::ProvenanceJournalFailed {
                            session_id: session_id.to_string(),
                            reason: format!("bound provenance graph is unavailable: {error}"),
                        }
                    })?;
                    (graph, hash, turn)
                }
                (None, None) => {
                    let frozen = sink.load_frozen_envelopes(&checkpoint).map_err(|reason| {
                        AgentError::ProvenanceJournalFailed {
                            session_id: session_id.to_string(),
                            reason,
                        }
                    })?;
                    let envelopes = frozen
                        .iter()
                        .map(|bytes| ProvenanceJournalEnvelope::from_json_bytes(bytes))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| AgentError::ProvenanceJournalFailed {
                            session_id: session_id.to_string(),
                            reason: error.to_string(),
                        })?;
                    let legacy_previous =
                        envelopes.iter().find_map(|envelope| match &envelope.event {
                            ProvenanceJournalEvent::LegacyGraphImport {
                                previous_provenance: Some(previous),
                                ..
                            } => Hash::from_base32(previous.as_bytes()),
                            _ => None,
                        });
                    let effective_previous =
                        checkpoint.source.previous_provenance.or(legacy_previous);
                    let todos = envelopes
                        .iter()
                        .filter_map(|envelope| match &envelope.event {
                            ProvenanceJournalEvent::Todo { todo } => Some(todo.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    let accumulator = ProvenanceAccumulator::replay_provenance_journal(envelopes)
                        .map_err(|error| AgentError::ProvenanceJournalFailed {
                        session_id: session_id.to_string(),
                        reason: error.to_string(),
                    })?;
                    let prepared = accumulator.prepare_provenance_graph(
                        &checkpoint.source.agent_name,
                        &checkpoint.source.agent_display_name,
                        &checkpoint.source.agent_vendor,
                        &checkpoint.source.change_hashes,
                    );
                    let mut graph = prepared.graph;
                    graph.previous = effective_previous;
                    graph.plan_id = checkpoint.source.plan_id.clone();
                    graph.todos = todos.clone();
                    let hash = change_store
                        .save_provenance_graph(&graph)
                        .map_err(|error| AgentError::ProvenanceJournalFailed {
                            session_id: session_id.to_string(),
                            reason: error.to_string(),
                        })?;
                    let turn = SessionTurn {
                        session_id: session_id.to_string(),
                        turn_number: checkpoint.source.ledger_turn_number,
                        goal: graph
                            .nodes
                            .iter()
                            .find(|node| {
                                matches!(
                                    node.kind,
                                    atomic_core::change::provenance_graph::ProvenanceNodeKind::Goal
                                )
                            })
                            .map(|node| node.summary.clone()),
                        provenance_hash: hash,
                        change_hashes: checkpoint.source.change_hashes.clone(),
                        previous_provenance: effective_previous,
                        timestamp: graph.timestamp,
                        plan_id: checkpoint.source.plan_id.clone(),
                        todos,
                    };
                    checkpoint = sink
                        .bind_checkpoint_hash(
                            &checkpoint,
                            hash,
                            turn.clone(),
                            event.timestamp.timestamp(),
                        )
                        .map_err(|reason| AgentError::ProvenanceJournalFailed {
                            session_id: session_id.to_string(),
                            reason,
                        })?;
                    (graph, hash, turn)
                }
                _ => {
                    return Err(AgentError::ProvenanceJournalFailed {
                        session_id: session_id.to_string(),
                        reason: "checkpoint has a partial hash binding".to_string(),
                    })
                }
            };

        let repository = atomic_repository::Repository::open_existing_wait(
            &self.repo_root,
            std::time::Duration::from_secs(10),
        )
        .map_err(|error| AgentError::ProvenanceJournalFailed {
            session_id: session_id.to_string(),
            reason: error.to_string(),
        })?;
        let publication = repository
            .publish_provenance_checkpoint(&graph, session_turn)
            .map_err(|error| AgentError::ProvenanceJournalFailed {
                session_id: session_id.to_string(),
                reason: error.to_string(),
            })?;
        sink.acknowledge_checkpoint(
            &checkpoint,
            publication.manifest_hash,
            event.timestamp.timestamp(),
        )
        .map_err(|reason| AgentError::ProvenanceJournalFailed {
            session_id: session_id.to_string(),
            reason,
        })?;

        Ok(())
    }
}

fn anonymous_tool_key(event: &TurnEvent) -> String {
    let bytes = serde_json::to_vec(&event.raw_json).unwrap_or_default();
    let digest = blake3::hash(&bytes).to_hex().to_string();
    format!("anonymous-{}", &digest[..16])
}

fn stable_journal_id(session_id: &str, turn_number: u32, discriminator: &str) -> String {
    let digest = blake3::hash(format!("{session_id}\0{turn_number}\0{discriminator}").as_bytes())
        .to_hex()
        .to_string();
    format!("hook-{}", &digest[..32])
}

fn reasoning_blocks(event: &TurnEvent) -> Vec<(String, Option<u64>, Option<String>)> {
    let Some(raw) = event.raw_json.as_ref() else {
        return Vec::new();
    };
    if let Some(blocks) = raw
        .get("reasoning_blocks")
        .and_then(serde_json::Value::as_array)
    {
        return blocks
            .iter()
            .filter_map(|block| {
                let text = block.get("text")?.as_str()?.trim();
                if text.is_empty() {
                    return None;
                }
                Some((
                    text.to_string(),
                    block.get("duration_ms").and_then(serde_json::Value::as_u64),
                    block
                        .get("signature")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned),
                ))
            })
            .collect();
    }

    let Some(text) = raw
        .get("reasoning_text")
        .and_then(serde_json::Value::as_str)
    else {
        return Vec::new();
    };
    let signature = raw
        .get("reasoning_signature")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let blocks: Vec<_> = text
        .split("\n---\n")
        .filter_map(|block| {
            let block = block.trim();
            (!block.is_empty()).then(|| block.to_string())
        })
        .collect();
    let last = blocks.len().saturating_sub(1);
    blocks
        .into_iter()
        .enumerate()
        .map(|(index, block)| {
            let block_signature = (index == last).then(|| signature.clone()).flatten();
            (block, None, block_signature)
        })
        .collect()
}

fn response_text(session: &AgentSession, event: &TurnEvent) -> Option<String> {
    let raw = event.raw_json.as_ref();
    let from_payload = |key: &str| -> Option<String> {
        raw.and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned)
    };
    from_payload("last_assistant_message")
        .or_else(|| from_payload("prompt_response"))
        .or_else(|| from_payload("response"))
        .or_else(|| {
            let path = session.transcript_path.as_ref()?;
            let data = std::fs::read(path).ok()?;
            transcript::last_assistant_text(
                &data,
                transcript::format_for_agent(&session.agent_name),
            )
        })
}

/// Extract the generic end-of-turn todo snapshot supplied by current agent
/// hooks. Preserve an upstream stable `id` when present. Agents that omit IDs
/// get a turn-local snapshot identity; this preserves the ledger faithfully
/// without falsely claiming cross-turn lifecycle continuity.
fn extract_turn_todos(event: &TurnEvent, session_id: &str, turn_number: u32) -> Vec<SessionTodo> {
    event
        .raw_json
        .as_ref()
        .and_then(|raw| raw.get("todos"))
        .and_then(serde_json::Value::as_array)
        .map(|todos| {
            todos
                .iter()
                .enumerate()
                .filter_map(|(index, value)| {
                    let content = value.get("content")?.as_str()?.to_string();
                    let id = value
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!("session:{}/turn:{}/todo:{}", session_id, turn_number, index)
                        });
                    Some(SessionTodo {
                        id,
                        content,
                        status: value
                            .get("status")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("pending")
                            .to_string(),
                        priority: value
                            .get("priority")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("medium")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod generic_context_tests {
    use super::*;
    use crate::event::HookType;

    #[test]
    fn extracts_explicit_and_turn_local_todo_ids() {
        let event = TurnEvent::new("sess-1", HookType::TurnEnd).with_raw_json(serde_json::json!({
            "todos": [
                {"id": "todo-7", "content": "Keep ID", "status": "in_progress", "priority": "high"},
                {"content": "Snapshot only", "status": "pending", "priority": "medium"}
            ]
        }));

        let todos = extract_turn_todos(&event, "sess-1", 3);
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].id, "todo-7");
        assert_eq!(todos[1].id, "session:sess-1/turn:3/todo:1");
        assert_eq!(todos[0].status, "in_progress");
    }
}
