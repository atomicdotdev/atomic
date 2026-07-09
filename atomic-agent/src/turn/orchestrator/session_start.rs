//! Session start handling for the turn orchestrator.

use crate::error::AgentResult;
use crate::event::TurnEvent;
use crate::turn::phase::{self, Action, Event, TransitionContext};
use crate::turn::session::AgentSession;

use super::{vendor_from_agent_name, DispatchResult, TurnOrchestrator};

impl TurnOrchestrator {
    /// Handle a SessionStart event.
    ///
    /// Creates a new session or re-enters an ended session. Creates the
    /// agent's Atomic view if needed (view creation is deferred to first
    /// recording since we don't want to create views for sessions that
    /// never produce changes).
    pub(super) async fn handle_session_start(
        &mut self,
        event: TurnEvent,
    ) -> AgentResult<DispatchResult> {
        let session_id = &event.session_id;

        // Determine whether this is a resume/continue (reuse existing view)
        // or a fresh startup (create a new view).
        //
        // Claude Code sends `source: "resume"` on `--continue` / `--resume`
        // with a NEW session_id (UUID), so a direct load by ID won't find
        // the previous session. We detect the resume intent and adopt the
        // most recent ended session's view instead of creating a new one.
        let is_resume = event
            .raw_json
            .as_ref()
            .and_then(|r| r.get("source").and_then(|v| v.as_str()))
            .map(|s| s == "resume" || s == "continue" || s == "compact" || s == "clear")
            .unwrap_or(false);

        // Load or create session
        let mut session = match self.session_store.load(session_id)? {
            Some(mut existing) => {
                // Re-entering an existing session (same session_id)
                let result = phase::transition(
                    existing.phase,
                    Event::SessionStart,
                    TransitionContext::default(),
                );
                let remaining = phase::apply_common_actions(&mut existing, &result);

                // Handle strategy-specific actions
                let mut warnings = Vec::new();
                for action in &remaining {
                    if let Action::WarnStaleSession = action {
                        warnings.push(format!(
                            "Session {} was already active — concurrent session detected",
                            session_id
                        ));
                    }
                }

                self.session_store.save(&existing)?;

                let mut dispatch = DispatchResult::new(session_id, existing.phase);
                for w in warnings {
                    dispatch = dispatch.with_warning(w);
                }

                return Ok(dispatch.with_message(format!(
                    "Atomic is tracking session {} (resumed, {} turn{} so far)",
                    session_id,
                    existing.turn_count,
                    if existing.turn_count == 1 { "" } else { "s" },
                )));
            }
            None if is_resume => {
                // Resume/continue with a NEW session_id — find the most
                // recent ended session and adopt its view so we don't
                // create a second orphan view.
                let recent = self
                    .session_store
                    .find_ended()?
                    .into_iter()
                    .max_by_key(|s| s.last_interaction);

                if let Some(prev) = recent {
                    log::info!(
                        "Resuming: new session {} adopting view '{}' from previous session {}",
                        session_id,
                        prev.view_name,
                        prev.session_id,
                    );

                    let mut session =
                        AgentSession::new(session_id, &self.agent_name, &self.agent_display_name);
                    session.view_name = prev.view_name.clone();
                    session.parent_view = prev.parent_view.clone();
                    session.turn_count = prev.turn_count;
                    session.files_touched = prev.files_touched.clone();

                    let vendor = vendor_from_agent_name(&self.agent_name);
                    session.agent_vendor = vendor.to_string();

                    session
                } else {
                    // No previous session found — fall through to new session
                    let mut session =
                        AgentSession::new(session_id, &self.agent_name, &self.agent_display_name);
                    let vendor = vendor_from_agent_name(&self.agent_name);
                    session.agent_vendor = vendor.to_string();
                    session
                }
            }
            None => {
                // Brand new session (source: "startup")
                let mut session =
                    AgentSession::new(session_id, &self.agent_name, &self.agent_display_name);

                // Set vendor from agent name. Model comes from the event's
                // raw_json (SessionStart sends it) — see below.
                let vendor = vendor_from_agent_name(&self.agent_name);
                session.agent_vendor = vendor.to_string();

                session
            }
        };

        // Only sessions CREATED here get the run stamp — the early return
        // above keeps pre-existing sessions out of the run's attribution.
        if let Some(stamp) = self.managed_stamp() {
            log::info!(
                "Session {} runs under managed lifecycle {} (owner: {})",
                session_id,
                stamp.run_id,
                stamp.owner_agent,
            );
            session.managed_run = Some(stamp);
        }

        // Set transcript path if provided
        if let Some(ref path) = event.transcript_path {
            session.set_transcript_path(path);
        }

        // Extract model and provider from SessionStart raw_json if present.
        // Claude Code sends: {"model": "claude-sonnet-4-5-20250929", "source": "startup", ...}
        // OpenCode sends:    {"model": "claude-opus-4-5", "provider": "anthropic", ...}
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

        // Fork the agent view from the current view.
        //
        // This ensures the agent inherits all existing changes (e.g.,
        // .atomicignore, project config, previously recorded code) instead
        // of starting from an empty graph. Without this, files that only
        // exist in the current view (like .atomicignore) would be invisible
        // to the agent's status/record workflow.
        //
        // Skip view creation for resumed sessions — the view already exists
        // from the previous session. We only need to switch to it.
        //
        // Best-effort: if the repo can't be opened or the view already
        // exists (resumed session), we log and continue — recording will
        // still work, it just won't have the parent's history.
        if session.parent_view.is_none() {
            match atomic_repository::Repository::open_existing(&self.repo_root) {
                Ok(repo) if repo.is_sandbox() => {
                    // A sandbox is a materialized copy of the project; `record`
                    // writes to the *canonical* graph (shared pristine +
                    // changes), on the view named in the `.atomic-sandbox`
                    // pointer. The agent is already on that view, so adopt it
                    // for recording and do NOT fork or switch:
                    //   * create_view_from would inject a spurious view into
                    //     the canonical graph, and
                    //   * set_current_view writes the *canonical*
                    //     .atomic/current_view, clobbering the real user's
                    //     current view.
                    let current = repo.current_view().to_string();
                    if let Some(declared) = self.managed_view() {
                        if declared != current {
                            log::debug!(
                                "Managed run declares view '{}' but the sandbox is \
                                 provisioned on '{}' — the sandbox pointer wins",
                                declared,
                                current,
                            );
                        }
                    }
                    log::info!(
                        "Sandbox session {}: recording on provisioned view '{}' (no fork/switch)",
                        session_id,
                        current,
                    );
                    session.view_name = current;
                }
                Ok(mut repo) => {
                    let current = repo.current_view().to_string();
                    session.set_parent_view(&current);

                    // Adopt the run's declared view instead of forking a
                    // per-session one. `create` below tolerates "already
                    // exists" for the second session of the same run.
                    if let Some(declared) = self.managed_view() {
                        log::info!(
                            "Session {} adopting managed-run view '{}' (declared by lifecycle)",
                            session_id,
                            declared,
                        );
                        session.view_name = declared.to_string();
                    }

                    match repo.create_view_from(&session.view_name, &current) {
                        Ok(()) => {
                            log::info!(
                                "Created agent view '{}' forked from '{}'",
                                session.view_name,
                                current,
                            );
                        }
                        Err(e) => {
                            // View may already exist (idempotent session start)
                            // or the source view may not exist yet (fresh repo).
                            // Either way, this is non-fatal.
                            log::debug!(
                                "Could not fork view '{}' from '{}': {} (non-fatal)",
                                session.view_name,
                                current,
                                e,
                            );
                        }
                    }

                    // Switch to the agent view so that all file writes during
                    // the session (tool calls, npm install, builds, etc.) happen
                    // while current_view points to the agent view. This ensures
                    // status/add/record see the right view. session-end will
                    // switch back to the user's view.
                    if let Err(e) = repo.align_to_view(&session.view_name) {
                        log::warn!(
                            "Could not align to agent view '{}': {} (non-fatal)",
                            session.view_name,
                            e,
                        );
                    } else {
                        log::info!("Aligned to agent view '{}'", session.view_name,);
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Could not open repository to fork agent view: {} (non-fatal)",
                        e,
                    );
                }
            }
        }

        // Save the new session
        self.session_store.save(&session)?;

        let message = format!(
            "Atomic is tracking this session. Each turn will be recorded as a change on view '{}'.",
            session.view_name,
        );

        Ok(DispatchResult::new(session_id, session.phase).with_message(message))
    }

    /// Load an existing session or create a new one.
    ///
    /// This is the resilient path — if a hook arrives for an unknown session
    /// (e.g., the session state was lost due to a crash, or `SessionStart`
    /// never fired because the agent doesn't support it), we create a new
    /// session and fork a proper draft view rather than failing.
    ///
    /// **Critical**: the fallback must fork a draft view parented on the
    /// user's current view, just like `handle_session_start` does.  If we
    /// skip the fork, `record()` later creates a shared/no-parent view
    /// whose universal filter exposes files from other views (e.g.
    /// `.atomicignore`, `.vault/*` from `dev`) that don't exist on disk,
    /// causing false "deleted" entries in `atomic status`.
    pub(crate) fn load_or_create_session(
        &self,
        session_id: &str,
        event: &TurnEvent,
    ) -> AgentResult<AgentSession> {
        match self.session_store.load(session_id)? {
            Some(session) => Ok(session),
            None => {
                log::info!(
                    "Creating session {} from {} event (no prior state found)",
                    session_id,
                    event.event_type,
                );
                let mut session =
                    AgentSession::new(session_id, &self.agent_name, &self.agent_display_name);

                let vendor = vendor_from_agent_name(&self.agent_name);
                session.agent_vendor = vendor.to_string();

                // Fallback-created sessions carry the same run stamp as
                // session-start-created ones.
                if let Some(stamp) = self.managed_stamp() {
                    session.managed_run = Some(stamp);
                }

                if let Some(ref path) = event.transcript_path {
                    session.set_transcript_path(path);
                }

                // Try to adopt the current view or fork a new draft view.
                //
                // If the current view looks like an agent view (not a
                // well-known shared view), adopt it — session-start already
                // created and switched to it.  Otherwise fork a new draft
                // view from the user's view so that the agent's changes
                // are isolated and the view filter only exposes the
                // parent's files.
                if let Ok(mut repo) = atomic_repository::Repository::open_existing(&self.repo_root)
                {
                    let current = repo.current_view().to_string();

                    if repo.is_sandbox() {
                        // Sandboxes already operate on the view named in their
                        // pointer file and record into the canonical graph.
                        // Adopt that view verbatim; never fork or switch
                        // (set_current_view writes the canonical
                        // .atomic/current_view the sandbox shares).
                        log::info!(
                            "Fallback sandbox session {} adopting provisioned view '{}'",
                            session_id,
                            current,
                        );
                        session.view_name = current;
                    } else if let Some(declared) = self.managed_view() {
                        // Adopt the run's declared view (same mechanics
                        // as handle_session_start).
                        log::info!(
                            "Fallback session {} adopting managed-run view '{}'",
                            session_id,
                            declared,
                        );
                        session.set_parent_view(&current);
                        session.view_name = declared.to_string();

                        if let Err(e) = repo.create_view_from(&session.view_name, &current) {
                            log::debug!(
                                "Fallback session {} could not fork view '{}' from '{}': {} (non-fatal)",
                                session_id,
                                session.view_name,
                                current,
                                e,
                            );
                        }

                        if let Err(e) = repo.set_current_view(&session.view_name) {
                            log::warn!(
                                "Fallback session {} could not switch to '{}': {} (non-fatal)",
                                session_id,
                                session.view_name,
                                e,
                            );
                        }
                    } else if current != "dev" && current != "main" && current != "release" {
                        // Adopt the existing agent view (session-start
                        // likely created it before this fallback ran).
                        log::info!(
                            "Fallback session {} adopting current view '{}'",
                            session_id,
                            current,
                        );
                        session.view_name = current;
                    } else {
                        // Fork a new draft view from the user's shared view.
                        // This mirrors what handle_session_start does.
                        session.set_parent_view(&current);

                        match repo.create_view_from(&session.view_name, &current) {
                            Ok(()) => {
                                log::info!(
                                    "Fallback session {} forked view '{}' from '{}'",
                                    session_id,
                                    session.view_name,
                                    current,
                                );
                            }
                            Err(e) => {
                                log::warn!(
                                    "Fallback session {} could not fork view '{}' from '{}': {} (non-fatal)",
                                    session_id,
                                    session.view_name,
                                    current,
                                    e,
                                );
                            }
                        }

                        // Switch to the agent view so status/add/record
                        // target the right view.
                        if let Err(e) = repo.align_to_view(&session.view_name) {
                            log::warn!(
                                "Fallback session {} could not align to '{}': {} (non-fatal)",
                                session_id,
                                session.view_name,
                                e,
                            );
                        }
                    }
                }

                // Persist so subsequent hooks find this session.
                self.session_store.save(&session)?;

                Ok(session)
            }
        }
    }
}
