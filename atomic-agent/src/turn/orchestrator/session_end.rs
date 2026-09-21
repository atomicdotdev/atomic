//! Session end handling for the turn orchestrator.

use crate::error::AgentResult;
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

        // Refusal is terminal and precedes watcher cancellation, view alignment,
        // status, flush-recording, provenance, and attestation. Repeated
        // session-end hooks return the same durable recovery object.
        if let Some(result) = self.persist_boundary_incomplete(session_id)? {
            return Ok(result);
        }

        let mut session = match self.session_store.load(session_id)? {
            Some(s) => s,
            None => {
                // Session not found — nothing to end
                log::info!("SessionEnd for unknown session {} — ignoring", session_id);
                return Ok(DispatchResult::new(session_id, Phase::Ended));
            }
        };

        let had_active_turn = session.is_turn_active();
        let journal_turn = if had_active_turn {
            session.turn_count.saturating_add(1)
        } else {
            session.turn_count.max(1)
        };
        if had_active_turn {
            self.commit_hook_event(&event, journal_turn)?;
        }

        // If a turn is still active, cancel the watcher
        if had_active_turn {
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
        let mut flush_incomplete: Option<crate::turn::session::IncompleteSession> = None;
        // Scoped sessions (dev sync): when hook file ownership narrows the
        // turn and no turn is in flight, alignment and flush are skipped —
        // there is nothing owned to record.
        if !session.explicit_record_files || had_active_turn {
            {
                // Ensure a non-sandbox working copy still desires the session's
                // agent view before recording. session-start aligns it, but that can
                // drift back to the parent view by session end (observed with
                // Cursor's CLI). A provisioned sandbox's persisted desired view is
                // authoritative, so adopt it instead of overwriting it.
                //
                // This also answers "is there a worktree to protect?" for the
                // failed-flush guard below, which is why it is captured rather than
                // discarded. See there.
                let has_worktree = match atomic_repository::Repository::open_existing(
                    &self.repo_root,
                ) {
                    Ok(mut repo) => {
                        if repo.is_sandbox() {
                            let desired_view = repo.current_view().to_string();
                            if desired_view != session.view_name {
                                log::warn!(
                                "SessionEnd: sandbox desired view '{}' differs from session view '{}'; adopting the persisted sandbox view",
                                desired_view,
                                session.view_name,
                            );
                                session.view_name = desired_view;
                            }
                        } else if repo.current_view() != session.view_name {
                            let working_copy = repo.require_working_copy_id();
                            match working_copy {
                            Ok(working_copy) => {
                                if let Err(e) =
                                    repo.align_to_view(working_copy, &session.view_name)
                                {
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
                            Err(e) => log::warn!(
                                "SessionEnd: could not resolve working-copy identity before aligning to agent view '{}': {} (non-fatal)",
                                session.view_name,
                                e,
                            ),
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

                // Flush only a turn actually in flight (review R3 fix-session):
                // headless agents leave an active turn at SessionEnd. When every
                // turn already recorded on `stop`, there is no pending turn —
                // and with the baseline cleared after each turn, running the
                // flush anyway would classify a fabricated baseline-less turn
                // and mark every cleanly-recorded session incomplete.
                if !session.is_turn_active() {
                    log::debug!(
                        "SessionEnd for session {}: no turn in flight, nothing to flush",
                        session_id
                    );
                } else {
                    let prompt = event
                        .prompt
                        .clone()
                        .or_else(|| session.current_prompt.clone())
                        .or_else(|| session.first_prompt.clone());
                    let turn_number = session.turn_count + 1;
                    let turn_duration_ms = session.current_turn_duration_ms().unwrap_or(0);
                    if had_active_turn {
                        self.commit_turn_completion_events(&session, &event, turn_number)?;
                    }
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
                        Ok(crate::record::TurnRecordResult::Recorded(outcome)) => {
                            session.end_turn();
                            let recorded_files: Vec<String> = outcome.recorded_file_list().to_vec();
                            session.add_files_touched(&recorded_files);
                            session.recorded_change_hashes.push(outcome.hash);
                            session.clear_current_prompt();
                            self.inject_reasoning_nodes(session_id, &event);
                            self.save_turn_provenance(
                                session_id, &session, &outcome, &event, None,
                            )?;
                            self.persist_content_turn_outcome(&mut session, &outcome, None);
                            // Review R2: a mixed flushed turn's unexplained Git
                            // transition refuses attribution just like a git-only
                            // turn, even though its content recorded.
                            if let Some(ref transition) = outcome.git_transition {
                                if let Some(ref incomplete) = transition.incomplete {
                                    flush_incomplete = Some(
                                        self.persist_incomplete_refusal(&mut session, incomplete),
                                    );
                                }
                            }
                            log::info!(
                                "SessionEnd flushed a pending turn for session {}: {}",
                                session_id,
                                outcome
                            );
                        }
                        Ok(crate::record::TurnRecordResult::Classified(classified)) => {
                            // RFC §10.2: a clean turn is classified, never empty.
                            session.end_turn();
                            self.persist_classified_turn(&mut session, &classified);
                            session.clear_current_prompt();
                            // Review R3 (executed probe): the refusal is durably
                            // persisted above, and it must ALSO surface in the
                            // dispatch result — the CLI turns that into a nonzero
                            // exit instead of a fresh success.
                            flush_incomplete = classified.incomplete.clone();
                            log::info!(
                                "SessionEnd for session {}: clean turn classified as {:?}",
                                session_id,
                                classified.outcome
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
                } // turn-in-flight flush guard
            }
        } // scoped-session skip gate

        if had_active_turn {
            self.stop_journal_turn(
                session_id,
                journal_turn,
                super::JournalStopCause::ProcessExited,
                true,
                event.timestamp.timestamp(),
            )?;
        }

        // State machine transition and finalization happen only after the
        // attestation succeeded: a signing/persistence failure blocks the
        // clean end (review R3/R6 — AC3 "block finalization on failure").
        // The refusal is durably persisted BEFORE the typed error returns,
        // and the CLI reports the failure as nonzero.
        if session.turn_count > 0 {
            if let Err(error) = self.create_session_attestation(&mut session) {
                let incomplete = crate::turn::session::IncompleteSession::new(
                    format!("session attestation failed: {error}"),
                    Vec::<String>::new(),
                    String::new(),
                    atomic_core::change::session::SessionIncompleteOrigin::UnfinalizedAttestation,
                );
                self.persist_incomplete_refusal(&mut session, &incomplete);
                self.session_store.save(&session)?;
                return Err(crate::error::AgentError::AttestationFailed {
                    session_id: session_id.to_string(),
                    reason: error.to_string(),
                });
            }
        }

        // State machine transition — the session is now cleanly ended with
        // its attestation saved (or nothing to attest).
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

        // Deliberately do NOT switch back to `session.parent_view`: the
        // working copy stays on the session's agent view so the user lands
        // where the work happened and can review it (`atomic log`/`diff`)
        // before deciding to insert it into a shared view. Switching back
        // forced users to hunt through `atomic view list` for the view
        // holding their agent's changes.

        let mut dispatch = DispatchResult::new(session_id, session.phase);
        if let Some(incomplete) = flush_incomplete {
            dispatch = dispatch.with_incomplete(incomplete);
        }
        Ok(dispatch)
    }
}
