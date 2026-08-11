//! Session end handling for the turn orchestrator.

use crate::error::{AgentError, AgentResult};
use crate::event::TurnEvent;
use crate::record::{record_turn, TurnRecordOptions};
use crate::turn::phase::{self, Event, Phase, TransitionContext};

use super::{DispatchResult, TurnOrchestrator};

impl TurnOrchestrator {
    /// Handle a SessionEnd event.
    ///
    /// Transitions the session to Ended, saves it, and creates an
    /// attestation covering all changes recorded during the session.
    ///
    /// The working copy stays on the session's agent view so the user
    /// lands where the work happened and can review it before inserting
    /// it into a shared view.
    ///
    /// The attestation is a graph-level audit node — it captures agent
    /// identity, timing, and which changes were recorded. Cost and token
    /// data are left at zero (they're not available from the hook) and
    /// can be enriched later via `atomic agent attest --enrich`.
    pub(super) async fn handle_session_end(
        &mut self,
        event: TurnEvent,
    ) -> AgentResult<DispatchResult> {
        let session_id = &event.session_id;

        let mut session = match self.session_store.load(session_id)? {
            Some(s) => s,
            None => {
                // Session not found — nothing to end
                log::info!("SessionEnd for unknown session {} — ignoring", session_id);
                return Ok(DispatchResult::new(session_id, Phase::Ended));
            }
        };

        // If a turn is still active, cancel the watcher
        if session.is_turn_active() {
            if let Err(e) = self.watcher.cancel_turn().await {
                log::warn!(
                    "Failed to cancel turn watcher for session {}: {}",
                    session_id,
                    e
                );
            }
        }

        // Flush any unrecorded turn BEFORE finalizing. Headless agents such as
        // Cursor's CLI fire sessionStart/postToolUse/sessionEnd but no per-turn
        // `stop` (TurnEnd), so the turn's file changes are still uncommitted at
        // session end. Record them now. Idempotent: agents that already
        // recorded each turn on `stop` leave a clean working copy here, so
        // `record_turn` returns `EmptyTurn` and this is a no-op for them.
        {
            // Ensure the working copy is on the session's agent view before
            // recording. session-start aligns to it, but that can drift back to
            // the parent view by session end (observed with Cursor's CLI), which
            // makes `record_turn` see a view mismatch and record nothing. Align
            // explicitly so the agent's uncommitted files are recorded on the
            // agent view. Best-effort — non-fatal if it can't switch.
            // Doubles as the "is there a worktree to protect?" answer for the
            // failed-flush guard below, which is why it is captured rather
            // than discarded. See there.
            let has_worktree = match atomic_repository::Repository::open_existing(&self.repo_root) {
                Ok(mut repo) => {
                    if repo.current_view() != session.view_name {
                        if let Err(e) = repo.align_to_view(&session.view_name) {
                            log::warn!(
                                "SessionEnd: could not align to agent view '{}': {} (non-fatal)",
                                session.view_name,
                                e,
                            );
                        } else {
                            log::info!(
                                "SessionEnd: aligned working copy to agent view '{}' before flush",
                                session.view_name,
                            );
                        }
                    }
                    true
                }
                Err(e) => {
                    log::warn!(
                        "SessionEnd: could not open repo to align view: {} (non-fatal)",
                        e
                    );
                    false
                }
            };

            let prompt = event
                .prompt
                .clone()
                .or_else(|| session.current_prompt.clone())
                .or_else(|| session.first_prompt.clone());
            let turn_number = session.turn_count + 1;
            let turn_duration_ms = session.current_turn_duration_ms().unwrap_or(0);
            let record_result = {
                let record_options = TurnRecordOptions {
                    session: &session,
                    event: &event,
                    turn_number,
                    turn_duration_ms,
                    prompt,
                };
                record_turn(&self.repo_root, &record_options)
            };
            match record_result {
                Ok(outcome) => {
                    session.end_turn();
                    let recorded_files: Vec<String> = outcome.recorded_file_list().to_vec();
                    session.add_files_touched(&recorded_files);
                    session.recorded_change_hashes.push(outcome.hash);
                    session.clear_current_prompt();
                    self.inject_reasoning_nodes(session_id, &event);
                    self.save_turn_provenance(session_id, &session, &outcome, &event);
                    log::info!(
                        "SessionEnd flushed a pending turn for session {}: {}",
                        session_id,
                        outcome
                    );
                }
                Err(AgentError::EmptyTurn { .. }) => {
                    log::info!(
                        "SessionEnd for session {}: no pending changes to flush",
                        session_id
                    );
                }
                Err(e) => {
                    log::error!(
                        "SessionEnd flush-record failed for session {}: {}",
                        session_id,
                        e
                    );
                    // Never finalize or attest a real repository after a
                    // failed flush. Keep the session active and leave the
                    // working copy on the agent view so its unrecorded work
                    // remains visible and recoverable. Repo-less orchestrator
                    // tests and integrations have no worktree to protect.
                    //
                    // This asks the question directly. It used to ask whether
                    // the session had a `parent_view`, which was a stand-in
                    // for the same thing — only the branch that opens a real
                    // repository set one — and a stand-in that quietly
                    // excluded sandboxes, whose unrecorded work is exactly
                    // what most needs protecting. It would also have changed
                    // meaning under anything that recorded a parent more
                    // often, which is a trap for the next reader.
                    if has_worktree {
                        return Err(e);
                    }
                }
            }
        }

        // State machine transition
        let result = phase::transition(
            session.phase,
            Event::SessionStop,
            TransitionContext::default(),
        );
        phase::apply_common_actions(&mut session, &result);

        self.session_store.save(&session)?;

        // Reconcile the Atomic session ledger: the session is now ended.
        // Best-effort — the JSON file remains the runtime fallback.
        let ended_at = session.ended_at.map(|t| t.timestamp());
        self.sync_session_lifecycle(&session, ended_at);

        log::info!(
            "Session {} ended after {} turn{}",
            session_id,
            session.turn_count,
            if session.turn_count == 1 { "" } else { "s" },
        );

        // Create an attestation for this session's changes.
        //
        // The attestation is a graph-level audit node that captures which
        // changes were recorded, by which agent, and when. Cost and token
        // data are left at zero — they're not available from the hook
        // payload. They can be enriched later when `claude --resume` data
        // is available.
        if session.turn_count > 0 {
            self.create_session_attestation(&session);
        }

        // Deliberately do NOT switch back to `session.parent_view`: the
        // working copy stays on the session's agent view so the user lands
        // where the work happened and can review it (`atomic log`/`diff`)
        // before deciding to insert it into a shared view. Switching back
        // forced users to hunt through `atomic view list` for the view
        // holding their agent's changes.

        Ok(DispatchResult::new(session_id, session.phase))
    }
}
