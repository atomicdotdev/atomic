//! Turn processing for the turn orchestrator.
//!
//! Contains handlers for TurnStart, TurnEnd, and ToolUse events.

use std::fs::File;
use std::path::Path;

use crate::error::{AgentError, AgentResult};
use crate::event::TurnEvent;
use crate::record::{record_turn, TurnRecordOptions};
use crate::turn::phase::{self, Action, Event, TransitionContext};

use super::{DispatchResult, TurnOrchestrator};

const TURN_END_LOCK_FILENAME: &str = "turn-end.lock";

struct TurnEndLockGuard {
    file: File,
}

impl Drop for TurnEndLockGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

enum TurnEndLock {
    Acquired(TurnEndLockGuard),
    Busy,
    Unavailable,
}

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
        let turn_number = session.turn_count.saturating_add(1);
        if session.managed_run.is_none() || self.managed_run.is_some() {
            self.resume_journal_turn(session_id, turn_number, event.timestamp.timestamp())?;
        }
        self.commit_hook_event(&event, turn_number)?;

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
        mut event: TurnEvent,
    ) -> AgentResult<DispatchResult> {
        // Clone so `event` stays mutable for enrichment below while the
        // id is borrowed throughout this function.
        let session_id_owned = event.session_id.clone();
        let session_id = session_id_owned.as_str();
        let _turn_end_lock = match self.try_turn_end_lock(session_id) {
            TurnEndLock::Acquired(guard) => Some(guard),
            TurnEndLock::Busy => {
                log::warn!(
                    "Turn end for session {} is already being recorded; skipping duplicate Stop hook",
                    session_id
                );
                return Ok(DispatchResult::new(session_id, phase::Phase::Idle)
                    .with_warning("duplicate Stop hook skipped: turn already recording"));
            }
            TurnEndLock::Unavailable => None,
        };

        // The session lock deduplicates one session's Stops. Independent
        // sessions (including sandboxes) still share pristine and must not
        // race status/add/record or checkpoint publication. Acquire before
        // reading session/status, and retain through publication and save.
        let _publication_lock = self.wait_turn_publication_lock(session_id)?;

        let mut session = self.load_or_create_session(session_id, &event)?;

        let has_changes = session.explicit_record_files || self.has_working_copy_changes();
        if !session.is_turn_active()
            && session.turn_count > 0
            && (session.explicit_record_files || !has_changes)
        {
            // A retried Stop after successful publication must not create a
            // second empty checkpoint for the same completed interaction.
            return Ok(DispatchResult::new(session_id, session.phase));
        }

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

        // Update the transcript path from the Stop event. Claude Code's Stop
        // payload carries `transcript_path`, but a session created without one
        // (transcript file not yet minted at SessionStart) would otherwise
        // record with `transcript_path = None` and lose the unhashed
        // `agent_turn` transcript data — the record below is the consumer.
        if let Some(path) = &event.transcript_path {
            session.set_transcript_path(path);
        }

        // OpenCode: recover transcript/reasoning/response from its local
        // store before recording — thin plugins send none of these, and
        // OpenCode writes no transcript file of its own.
        self.enrich_opencode_turn(&mut session, &mut event)?;
        let pending_turn_number = session.turn_count.saturating_add(1);
        self.commit_turn_completion_events(&session, &event, pending_turn_number)?;

        // A read-only turn still owns a journal turn number and immutable
        // provenance. Finalize it before advancing the session so the next
        // prompt cannot be deduplicated against this turn's goal/response IDs.
        if !has_changes {
            if self.watcher.is_active() {
                let _ = self.watcher.cancel_turn().await;
            }
            session.end_turn();
            self.checkpoint_turn_provenance(session_id, &session, &[], &event)?;
            session.clear_current_prompt();
            let result =
                phase::transition(session.phase, Event::TurnEnd, TransitionContext::default());
            phase::apply_common_actions(&mut session, &result);
            self.session_store.save(&session)?;
            return Ok(DispatchResult::new(session_id, session.phase));
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

        // Execute strategy-specific actions. Record the session's view so the
        // caller can locate the change, which lands on the agent view rather
        // than the default view.
        let mut dispatch =
            DispatchResult::new(session_id, session.phase).with_view(&session.view_name);

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

                            // Track the change hash so the session-end attestation
                            // covers only what the agent actually recorded — not
                            // inherited baseline changes from the parent view.
                            session.recorded_change_hashes.push(outcome.hash);

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
                                self.ingest_sherpa_trace(
                                    session_id,
                                    turn_number,
                                    Path::new(trace_path),
                                )?;
                            }

                            // Provenance: inject reasoning blocks as Decision nodes
                            // and the agent's closing message as an LlmResponse node,
                            // then append a patch proposal node and save the graph.
                            self.save_turn_provenance(session_id, &session, &outcome, &event)?;

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
                            self.checkpoint_turn_provenance(session_id, &session, &[], &event)?;
                            session.clear_current_prompt();
                        }
                        Err(e) => {
                            log::error!(
                                "Failed to record turn {} for session {}: {}",
                                turn_number,
                                session_id,
                                e
                            );
                            if self.journal_sink.is_some() {
                                // Preserve the active session and durable journal
                                // for retry; a failed record is not a finished turn.
                                return Err(e);
                            }
                            // Legacy orchestrators without a durable journal
                            // retain their best-effort recording behavior.
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

        // Agents without a SessionEnd lifecycle hook (Antigravity CLI) never
        // trigger `handle_session_end`, so their attestations would never be
        // created. Their Stop payload carries `fullyIdle: true` when the
        // execution loop fully terminated — that is their terminal signal.
        // `create_session_attestation` is incremental (covers only newly
        // recorded hashes) and chained via `previous_attestation`, so
        // calling it after every idle Stop is safe and idempotent.
        let fully_idle = event
            .raw_json
            .as_ref()
            .and_then(|r| r.get("fullyIdle"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if fully_idle {
            self.create_session_attestation(&session);
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

        let session = self.load_or_create_session(session_id, &event)?;
        let turn_number = session.turn_count.saturating_add(1);
        self.commit_hook_event(&event, turn_number)?;

        // Log tool usage
        if let Some(ref tool_name) = event.tool_name {
            log::info!(
                "Session {} tool use: {} ({})",
                session_id,
                tool_name,
                event.event_type,
            );
        }

        Ok(DispatchResult::new(session_id, session.phase))
    }

    fn wait_turn_publication_lock(
        &self,
        session_id: &str,
    ) -> AgentResult<Option<TurnEndLockGuard>> {
        use fs2::FileExt;
        let failure = |reason: String| AgentError::ProvenanceJournalFailed {
            session_id: session_id.to_owned(),
            reason,
        };
        let canonical = match atomic_repository::Repository::canonical_dot_dir(&self.repo_root) {
            Ok(path) => path,
            // Legacy session-only orchestrators can run without a repository.
            // A journal-backed Stop must always have canonical coordination.
            Err(
                atomic_repository::RepositoryError::NotFound { .. }
                | atomic_repository::RepositoryError::NotInRepository,
            ) if self.journal_sink.is_none() => return Ok(None),
            Err(error) => return Err(failure(error.to_string())),
        };
        // Never unlink a lock file: waiters must all lock the same inode.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(canonical.join("turn-publication.lock"))
            .map_err(|error| failure(error.to_string()))?;
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(10);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Some(TurnEndLockGuard { file })),
                Err(error) if super::is_lock_contended(&error) => {
                    if start.elapsed() >= timeout {
                        return Err(failure(
                            "timed out waiting for another Stop to publish; retry this Stop".into(),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(failure(error.to_string())),
            }
        }
    }

    fn try_turn_end_lock(&self, session_id: &str) -> TurnEndLock {
        use fs2::FileExt;

        let dir = self.session_graph_dir(session_id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!(
                "Failed to create turn-end lock dir for session {}: {}",
                session_id,
                e
            );
            return TurnEndLock::Unavailable;
        }

        let lock_path = dir.join(TURN_END_LOCK_FILENAME);
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(e) => {
                log::warn!(
                    "Failed to open turn-end lock for session {}: {}",
                    session_id,
                    e
                );
                return TurnEndLock::Unavailable;
            }
        };

        match file.try_lock_exclusive() {
            Ok(()) => TurnEndLock::Acquired(TurnEndLockGuard { file }),
            Err(e) if super::is_lock_contended(&e) => TurnEndLock::Busy,
            Err(e) => {
                log::warn!(
                    "Failed to acquire turn-end lock for session {}: {}",
                    session_id,
                    e
                );
                TurnEndLock::Unavailable
            }
        }
    }
}
