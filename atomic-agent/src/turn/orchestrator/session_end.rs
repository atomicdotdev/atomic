//! Session end handling for the turn orchestrator.

use crate::error::AgentResult;
use crate::event::TurnEvent;
use crate::turn::phase::{self, Event, Phase, TransitionContext};

use super::{DispatchResult, TurnOrchestrator};

impl TurnOrchestrator {
    /// Handle a SessionEnd event.
    ///
    /// Transitions the session to Ended, saves it, and creates an
    /// attestation covering all changes recorded during the session.
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

        // State machine transition
        let result = phase::transition(
            session.phase,
            Event::SessionStop,
            TransitionContext::default(),
        );
        phase::apply_common_actions(&mut session, &result);

        self.session_store.save(&session)?;

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

        // Switch back to the user's original view. session-start switched
        // to the agent view so that all file writes happened there. Now
        // that the session is over, restore the user's view.
        if let Some(ref parent) = session.parent_view {
            match atomic_repository::Repository::open_existing(&self.repo_root) {
                Ok(mut repo) => {
                    if let Err(e) = repo.switch_view(parent) {
                        log::warn!(
                            "Could not switch back to user view '{}': {} (non-fatal)",
                            parent,
                            e,
                        );
                    } else {
                        log::info!("Restored user view '{}'", parent);
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Could not open repository to restore user view: {} (non-fatal)",
                        e,
                    );
                }
            }
        }

        Ok(DispatchResult::new(session_id, session.phase))
    }
}
