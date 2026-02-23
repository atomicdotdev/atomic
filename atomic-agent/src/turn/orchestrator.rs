//! Turn orchestrator — dispatches events through state machine and recording.
//!
//! The `TurnOrchestrator` is the central coordinator that connects:
//!
//! - **Agent hooks** (via [`TurnEvent`]) — lifecycle events from the agent
//! - **State machine** (via [`transition`]) — determines what actions to take
//! - **File watcher** (via [`FileWatcher`]) — optional real-time file tracking
//! - **Recording** (via [`record_turn`]) — status → add → record workflow
//! - **Session store** (via [`SessionStore`]) — persists session state
//!
//! # Event Dispatch Flow
//!
//! ```text
//! TurnEvent (from agent hook stdin)
//!     │
//!     ▼
//! TurnOrchestrator::dispatch(event)
//!     │
//!     ├─ SessionStart → create/resume session, create stack
//!     ├─ TurnStart    → watcher.begin_turn(), transition to Active
//!     ├─ TurnEnd      → watcher.end_turn(), transition to Idle, record_turn()
//!     ├─ SessionEnd   → transition to Ended, save session
//!     └─ ToolUse      → track tool usage (sub-turn granularity)
//! ```
//!
//! # Error Recovery
//!
//! The orchestrator is designed to be resilient to crashes:
//!
//! - **Ctrl-C recovery**: If `TurnStart` arrives while `Active`, the state
//!   machine treats it as Ctrl-C recovery and continues (previous turn was
//!   interrupted).
//! - **Missing session**: If a hook arrives for an unknown session ID, a new
//!   session is created automatically.
//! - **Watcher failure**: If the watcher fails, the error is logged but the
//!   session state is still updated.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_agent::turn::orchestrator::TurnOrchestrator;
//! use atomic_agent::event::{HookType, TurnEvent};
//!
//! let mut orchestrator = TurnOrchestrator::new("/path/to/repo").await?;
//!
//! // Agent session starts
//! let event = TurnEvent::new("sess-123", HookType::SessionStart);
//! orchestrator.dispatch(event).await?;
//!
//! // Agent turn starts (user submitted a prompt)
//! let event = TurnEvent::new("sess-123", HookType::TurnStart)
//!     .with_prompt("Fix the authentication bug");
//! orchestrator.dispatch(event).await?;
//!
//! // Agent turn ends (agent finished responding)
//! let event = TurnEvent::new("sess-123", HookType::TurnEnd);
//! let result = orchestrator.dispatch(event).await?;
//! // result.change_hash is Some if files changed and a change was recorded
//! ```

use std::path::{Path, PathBuf};

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::provenance::accumulator::ProvenanceAccumulator;
use crate::record::{record_turn, TurnRecordOptions, TurnRecordOutcome};
use crate::turn::phase::{self, Action, Event, Phase, TransitionContext};
use crate::turn::session::{AgentSession, SessionStore};
use crate::watcher::{self, FileWatcher, WatcherConfig};

// DispatchResult

/// The result of dispatching an event through the orchestrator.
///
/// Contains optional data depending on what happened during dispatch:
/// - `change_recorded` is set when a turn was recorded as an Atomic change
/// - `message` contains a user-facing message (for hook stdout responses)
/// - `session_id` is always set to the session that was affected
#[derive(Debug)]
pub struct DispatchResult {
    /// The session ID that was affected.
    pub session_id: String,

    /// The new phase after the transition.
    pub new_phase: Phase,

    /// If a turn was recorded, contains the outcome.
    pub change_recorded: Option<TurnRecordOutcome>,

    /// User-facing message to return to the agent (via hook stdout).
    ///
    /// For `SessionStart`, this is the "Atomic is tracking" message.
    /// For other events, this is typically `None`.
    pub message: Option<String>,

    /// Whether any warnings were emitted (e.g., stale session).
    pub warnings: Vec<String>,
}

impl DispatchResult {
    /// Create a new result with just a session ID and phase.
    ///
    /// This is public so that other crates (e.g., `atomic-cli`) can construct
    /// `DispatchResult` values for testing hook response handling.
    pub fn new(session_id: impl Into<String>, phase: Phase) -> Self {
        Self {
            session_id: session_id.into(),
            new_phase: phase,
            change_recorded: None,
            message: None,
            warnings: Vec::new(),
        }
    }

    /// Set the recorded change outcome.
    fn with_change(mut self, outcome: TurnRecordOutcome) -> Self {
        self.change_recorded = Some(outcome);
        self
    }

    /// Set the user-facing message.
    fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Add a warning.
    fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    /// Returns `true` if a change was recorded during this dispatch.
    pub fn was_recorded(&self) -> bool {
        self.change_recorded.is_some()
    }
}

impl std::fmt::Display for DispatchResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] → {}", self.session_id, self.new_phase)?;
        if let Some(ref outcome) = self.change_recorded {
            write!(f, " ({})", outcome)?;
        }
        for warning in &self.warnings {
            write!(f, " ⚠ {}", warning)?;
        }
        Ok(())
    }
}

// TurnOrchestrator

/// Central coordinator for agent turn lifecycle.
///
/// Manages the session store, file watcher, and dispatches events through
/// the state machine to produce Atomic changes.
///
/// # Lifecycle
///
/// Create one orchestrator per hook invocation. Each hook call:
/// 1. Creates a `TurnOrchestrator` (opens session store, creates watcher)
/// 2. Calls `dispatch(event)` with the parsed hook event
/// 3. Reads the `DispatchResult` for any data to send back to the agent
///
/// The orchestrator does NOT persist across hook calls — each invocation is
/// independent. Session state is persisted to disk via `SessionStore`.
pub struct TurnOrchestrator {
    /// Path to the repository root (where `.atomic/` lives).
    repo_root: PathBuf,

    /// Persistent session state storage.
    session_store: SessionStore,

    /// File change detection backend.
    watcher: Box<dyn FileWatcher>,

    /// Agent registry key (e.g., "claude-code", "gemini-cli").
    ///
    /// Set by the CLI hook handler so that sessions created by this
    /// orchestrator get the correct agent name instead of "unknown".
    agent_name: String,

    /// Human-readable agent display name (e.g., "Claude Code").
    agent_display_name: String,
}

impl TurnOrchestrator {
    /// Create a new orchestrator for the given repository.
    ///
    /// Opens the session store at `.atomic/sessions/` and creates a file
    /// watcher (Watchman if available, fallback otherwise).
    ///
    /// # Arguments
    ///
    /// * `repo_root` — Path to the repository root (where `.atomic/` lives)
    ///
    /// # Errors
    ///
    /// Returns an error if the session store directory cannot be created
    /// or if the file watcher fails to initialize.
    pub async fn new(repo_root: impl Into<PathBuf>) -> AgentResult<Self> {
        let repo_root = repo_root.into();

        let session_store = SessionStore::for_repo(&repo_root)?;

        let watcher_config = WatcherConfig::new(&repo_root);
        let watcher = watcher::create_watcher(watcher_config).await?;

        Ok(Self {
            repo_root,
            session_store,
            watcher,
            agent_name: "unknown".to_string(),
            agent_display_name: "Unknown Agent".to_string(),
        })
    }

    /// Create an orchestrator with an explicit watcher (for testing).
    ///
    /// This allows injecting a mock or custom watcher instead of using
    /// the auto-detected one.
    pub fn with_watcher(
        repo_root: impl Into<PathBuf>,
        session_store: SessionStore,
        watcher: Box<dyn FileWatcher>,
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            session_store,
            watcher,
            agent_name: "unknown".to_string(),
            agent_display_name: "Unknown Agent".to_string(),
        }
    }

    /// Set the agent identity for sessions created by this orchestrator.
    ///
    /// Called by the CLI hook handler after construction, before dispatching
    /// events. This ensures new sessions get the correct agent name
    /// (e.g., "claude-code" / "Claude Code") instead of "unknown".
    pub fn set_agent(&mut self, name: impl Into<String>, display_name: impl Into<String>) {
        self.agent_name = name.into();
        self.agent_display_name = display_name.into();
    }

    /// Returns the repository root path.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Returns a reference to the session store.
    pub fn session_store(&self) -> &SessionStore {
        &self.session_store
    }

    // Main dispatch

    /// Dispatch a turn event through the orchestrator.
    ///
    /// This is the main entry point called by the CLI hook handler. It:
    /// 1. Routes the event to the appropriate handler based on `event_type`
    /// 2. Loads or creates the session state
    /// 3. Runs the state machine transition
    /// 4. Executes any actions (begin/end turn, record change)
    /// 5. Saves the updated session state
    /// 6. Returns a `DispatchResult` with any data to send back to the agent
    ///
    /// # Arguments
    ///
    /// * `event` — The normalized event from the agent hook
    ///
    /// # Errors
    ///
    /// Most errors are non-fatal — the orchestrator logs warnings and
    /// continues. Fatal errors (session store IO failure, etc.) are propagated.
    pub async fn dispatch(&mut self, event: TurnEvent) -> AgentResult<DispatchResult> {
        match event.event_type {
            HookType::SessionStart => self.handle_session_start(event).await,
            HookType::TurnStart => self.handle_turn_start(event).await,
            HookType::TurnEnd => self.handle_turn_end(event).await,
            HookType::SessionEnd => self.handle_session_end(event).await,
            HookType::PreToolUse | HookType::PostToolUse => self.handle_tool_use(event).await,
        }
    }

    // Event handlers

    /// Handle a SessionStart event.
    ///
    /// Creates a new session or re-enters an ended session. Creates the
    /// agent's Atomic stack if needed (stack creation is deferred to first
    /// recording since we don't want to create stacks for sessions that
    /// never produce changes).
    async fn handle_session_start(&mut self, event: TurnEvent) -> AgentResult<DispatchResult> {
        let session_id = &event.session_id;

        // Load or create session
        let mut session = match self.session_store.load(session_id)? {
            Some(mut existing) => {
                // Re-entering an existing session
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
            None => {
                // New session — use the agent identity set by the CLI hook handler
                let mut session =
                    AgentSession::new(session_id, &self.agent_name, &self.agent_display_name);

                // Set vendor from agent name. Model comes from the event's
                // raw_json (SessionStart sends it) — see below.
                let vendor = vendor_from_agent_name(&self.agent_name);
                session.agent_vendor = vendor.to_string();

                session
            }
        };

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

        // Fork the agent stack from the current stack.
        //
        // This ensures the agent inherits all existing changes (e.g.,
        // .atomicignore, project config, previously recorded code) instead
        // of starting from an empty graph. Without this, files that only
        // exist in the current stack (like .atomicignore) would be invisible
        // to the agent's status/record workflow.
        //
        // Best-effort: if the repo can't be opened or the stack already
        // exists (resumed session), we log and continue — recording will
        // still work, it just won't have the parent's history.
        if session.parent_stack.is_none() {
            match atomic_repository::Repository::open(&self.repo_root) {
                Ok(mut repo) => {
                    let current = repo.current_stack().to_string();
                    session.set_parent_stack(&current);

                    match repo.create_stack_from(&session.stack_name, &current) {
                        Ok(()) => {
                            log::info!(
                                "Created agent stack '{}' forked from '{}'",
                                session.stack_name,
                                current,
                            );
                        }
                        Err(e) => {
                            // Stack may already exist (idempotent session start)
                            // or the source stack may not exist yet (fresh repo).
                            // Either way, this is non-fatal.
                            log::debug!(
                                "Could not fork stack '{}' from '{}': {} (non-fatal)",
                                session.stack_name,
                                current,
                                e,
                            );
                        }
                    }

                    // Switch to the agent stack so that all file writes during
                    // the session (tool calls, npm install, builds, etc.) happen
                    // while current_stack points to the agent stack. This ensures
                    // status/add/record see the right view. session-end will
                    // switch back to the user's stack.
                    if let Err(e) = repo.set_current_stack(&session.stack_name) {
                        log::warn!(
                            "Could not switch to agent stack '{}': {} (non-fatal)",
                            session.stack_name,
                            e,
                        );
                    } else {
                        log::info!("Switched to agent stack '{}'", session.stack_name,);
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Could not open repository to fork agent stack: {} (non-fatal)",
                        e,
                    );
                }
            }
        }

        // Save the new session
        self.session_store.save(&session)?;

        let message = format!(
            "Atomic is tracking this session. Each turn will be recorded as a change on stack '{}'.",
            session.stack_name,
        );

        Ok(DispatchResult::new(session_id, session.phase).with_message(message))
    }

    /// Handle a TurnStart event (UserPromptSubmit).
    ///
    /// Begins file watching and transitions the session to Active.
    async fn handle_turn_start(&mut self, event: TurnEvent) -> AgentResult<DispatchResult> {
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
    async fn handle_turn_end(&mut self, event: TurnEvent) -> AgentResult<DispatchResult> {
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

        // Compute turn metadata
        let turn_duration_ms = session.current_turn_duration_ms().unwrap_or(0);
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
                    // Get the prompt from the event or session
                    let prompt = event
                        .prompt
                        .clone()
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

                            // Provenance: append a patch proposal node and save
                            // the provenance graph to the repository.
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

    /// Handle a SessionEnd event.
    ///
    /// Transitions the session to Ended, saves it, and creates an
    /// attestation covering all changes recorded during the session.
    ///
    /// The attestation is a graph-level audit node — it captures agent
    /// identity, timing, and which changes were recorded. Cost and token
    /// data are left at zero (they're not available from the hook) and
    /// can be enriched later via `atomic agent attest --enrich`.
    async fn handle_session_end(&mut self, event: TurnEvent) -> AgentResult<DispatchResult> {
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

        // Switch back to the user's original stack. session-start switched
        // to the agent stack so that all file writes happened there. Now
        // that the session is over, restore the user's view.
        if let Some(ref parent) = session.parent_stack {
            match atomic_repository::Repository::open(&self.repo_root) {
                Ok(mut repo) => {
                    if let Err(e) = repo.switch_stack(parent) {
                        log::warn!(
                            "Could not switch back to user stack '{}': {} (non-fatal)",
                            parent,
                            e,
                        );
                    } else {
                        log::info!("Restored user stack '{}'", parent);
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Could not open repository to restore user stack: {} (non-fatal)",
                        e,
                    );
                }
            }
        }

        Ok(DispatchResult::new(session_id, session.phase))
    }

    /// Handle a PreToolUse or PostToolUse event.
    ///
    /// Currently tracks tool usage for informational purposes. Future
    /// enhancements may create sub-turn recordings for sub-agent (Task) tools.
    async fn handle_tool_use(&mut self, event: TurnEvent) -> AgentResult<DispatchResult> {
        let session_id = &event.session_id;

        let session = match self.session_store.load(session_id)? {
            Some(s) => s,
            None => {
                log::debug!("ToolUse for unknown session {} — ignoring", session_id);
                return Ok(DispatchResult::new(session_id, Phase::Idle));
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

                // Extract tool_input, tool_output, status, duration from raw_json
                let raw = event.raw_json.as_ref();
                let tool_input = raw.and_then(|r| r.get("tool_input"));
                let tool_output = raw.and_then(|r| r.get("tool_output").and_then(|v| v.as_str()));
                let status = raw.and_then(|r| r.get("status").and_then(|v| v.as_str()));
                let duration_ms = raw.and_then(|r| r.get("duration").and_then(|v| v.as_u64()));

                acc.append_tool_call(
                    tool_name,
                    tool_call_id,
                    tool_input,
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

    // Helpers

    /// Load an existing session or create a new one.
    ///
    /// This is the resilient path — if a hook arrives for an unknown session
    /// (e.g., the session state was lost due to a crash), we create a new
    /// session rather than failing.
    fn load_or_create_session(
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

                if let Some(ref path) = event.transcript_path {
                    session.set_transcript_path(path);
                }

                Ok(session)
            }
        }
    }
}

/// Infer vendor from the agent registry name.
///
/// The model comes from the agent's hook payload (e.g., SessionStart sends
/// `"model": "claude-sonnet-4-5-20250929"`). We don't guess the model —
/// only the vendor, which is deterministic from the agent name.
impl TurnOrchestrator {
    // =========================================================================
    // Provenance graph helpers
    // =========================================================================

    /// Get the session directory for provenance graph storage.
    ///
    /// Returns `{sessions_dir}/{session_id}/` — a subdirectory alongside
    /// the session's JSON file. The `ProvenanceAccumulator` stores its
    /// `graph.json` here.
    fn session_graph_dir(&self, session_id: &str) -> PathBuf {
        self.session_store.sessions_dir().join(session_id)
    }

    /// Load or create the provenance accumulator for a session.
    ///
    /// Best-effort: returns `None` on failure (logged, never fatal).
    fn load_accumulator(&self, session_id: &str) -> Option<ProvenanceAccumulator> {
        let dir = self.session_graph_dir(session_id);
        match ProvenanceAccumulator::load_or_create(&dir, session_id) {
            Ok(acc) => Some(acc),
            Err(e) => {
                log::warn!(
                    "Failed to load provenance accumulator for {}: {}",
                    session_id,
                    e,
                );
                None
            }
        }
    }

    /// Save the provenance accumulator for a session.
    ///
    /// Best-effort: failures are logged but never fatal.
    fn save_accumulator(&self, session_id: &str, acc: &ProvenanceAccumulator) {
        let dir = self.session_graph_dir(session_id);
        if let Err(e) = acc.save(&dir) {
            log::warn!(
                "Failed to save provenance accumulator for {}: {}",
                session_id,
                e,
            );
        }
    }

    /// Save the provenance graph for a recorded turn.
    ///
    /// Appends a patch proposal node to the accumulator, converts to a
    /// content-addressed `ProvenanceGraph`, and saves it to the repository.
    /// The accumulator's `last_provenance_hash` is updated so subsequent
    /// graphs chain correctly via `previous`.
    ///
    /// Best-effort: all failures are logged but never block the session.
    fn save_turn_provenance(
        &self,
        session_id: &str,
        session: &AgentSession,
        outcome: &TurnRecordOutcome,
        event: &TurnEvent,
    ) {
        use atomic_core::types::Base32;

        let mut acc = match self.load_accumulator(session_id) {
            Some(a) => a,
            None => return,
        };

        // Append a patch proposal node for the recorded change
        let change_hash_base32 = outcome.hash.to_base32();
        acc.append_patch_proposal(
            &change_hash_base32,
            &outcome.recorded_file_list().to_vec(),
            event.timestamp.timestamp(),
        );

        // Convert the accumulated graph to a content-addressed ProvenanceGraph
        let change_hashes = vec![outcome.hash];
        let graph = acc.to_provenance_graph(
            &session.agent_name,
            &session.agent_display_name,
            &session.agent_vendor,
            &change_hashes,
        );

        // Save to the repository
        match atomic_repository::Repository::open(&self.repo_root) {
            Ok(repo) => match repo.save_provenance_graph(&graph) {
                Ok(hash) => {
                    acc.set_last_provenance_hash(hash.to_base32());
                    log::info!(
                        "Saved provenance graph {} for session {} ({} nodes, {} edges, {} changes)",
                        hash.to_base32(),
                        session_id,
                        graph.node_count(),
                        graph.edge_count(),
                        graph.change_count(),
                    );
                }
                Err(e) => {
                    log::warn!(
                        "Failed to save provenance graph for session {}: {}",
                        session_id,
                        e,
                    );
                }
            },
            Err(e) => {
                log::warn!(
                    "Could not open repository to save provenance graph for session {}: {}",
                    session_id,
                    e,
                );
            }
        }

        // Persist the updated accumulator (with last_provenance_hash)
        self.save_accumulator(session_id, &acc);
    }

    // =========================================================================
    // Attestation
    // =========================================================================

    /// Create an attestation covering changes in a session's agent stack.
    ///
    /// On a fresh session, this covers all changes in the stack. On a
    /// resumed session (`claude --resume`), this only covers the NEW
    /// changes that aren't already covered by a previous attestation,
    /// and chains to that attestation via `previous_attestation`.
    ///
    /// The attestation is enriched with data from the provenance entries
    /// embedded in each covered change: model name, token counts, cost,
    /// and line-level code change statistics. This data was recorded by
    /// `build_turn_provenance()` at `record_turn()` time.
    ///
    /// Best-effort: if anything fails (repo can't open, stack doesn't exist,
    /// no changes recorded), we log and continue — the session still ended
    /// successfully.
    fn create_session_attestation(&self, session: &AgentSession) {
        use atomic_core::change::attestation::{
            AttestAgent, Attestation, CodeChangeStats, ModelUsage,
        };
        use atomic_core::types::Base32;
        use std::collections::{HashMap, HashSet};

        // Open the repository
        let repo = match atomic_repository::Repository::open(&self.repo_root) {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Could not open repository to create attestation for session {}: {}",
                    session.session_id,
                    e,
                );
                return;
            }
        };

        // Query the agent stack for all change hashes
        let history = match repo
            .log(atomic_repository::history::HistoryOptions::default().stack(&session.stack_name))
        {
            Ok(h) => h,
            Err(e) => {
                log::debug!(
                    "Could not read history for stack '{}': {} (no attestation created)",
                    session.stack_name,
                    e,
                );
                return;
            }
        };

        if history.is_empty() {
            log::debug!(
                "Stack '{}' has no changes — skipping attestation",
                session.stack_name,
            );
            return;
        }

        let all_change_hashes: Vec<atomic_core::types::Hash> =
            history.iter().map(|e| e.hash).collect();

        // Handle resumed sessions: find existing attestations and determine
        // which changes are new (not yet covered by any attestation).
        //
        // On a fresh session, all changes are new.
        // On a resumed session, only the changes added since the last
        // attestation need to be covered. The new attestation chains to
        // the most recent existing one via `previous_attestation`.
        let mut already_covered: HashSet<atomic_core::types::Hash> = HashSet::new();
        let mut previous_attestation: Option<atomic_core::types::Hash> = None;
        let mut latest_attest_timestamp: i64 = 0;

        // Check each change for existing attestations
        for change_hash in &all_change_hashes {
            let attestations = repo
                .find_attestations_for_change(change_hash)
                .unwrap_or_default();

            for (attest_hash, attest) in &attestations {
                // Only consider attestations from the same session
                if attest.session_id != session.session_id {
                    continue;
                }

                // Track all changes already covered by this session's attestations
                for covered in &attest.changes_covered {
                    already_covered.insert(*covered);
                }

                // Track the most recent attestation for chaining
                if attest.timestamp > latest_attest_timestamp {
                    latest_attest_timestamp = attest.timestamp;
                    previous_attestation = Some(*attest_hash);
                }
            }
        }

        // Determine which changes are new (not covered by existing attestations)
        let new_change_hashes: Vec<atomic_core::types::Hash> = all_change_hashes
            .iter()
            .filter(|h| !already_covered.contains(h))
            .cloned()
            .collect();

        if new_change_hashes.is_empty() {
            log::info!(
                "All {} changes in session {} are already attested — skipping",
                all_change_hashes.len(),
                session.session_id,
            );
            return;
        }

        let is_resume = previous_attestation.is_some();

        // Compute wall duration from session timestamps
        let wall_duration_ms = session
            .ended_at
            .map(|ended| {
                let duration = ended - session.started_at;
                duration.num_milliseconds().max(0) as u64
            })
            .unwrap_or(0);

        // Aggregate provenance data from the covered changes.
        //
        // Each change carries provenance entries (model, tokens, cost) set by
        // `build_turn_provenance()` at record time. We aggregate across all
        // covered changes to populate the attestation with real data instead
        // of leaving it at zeros.
        let mut total_cost: f64 = 0.0;
        let total_api_ms: u64 = 0;
        let mut lines_added: u64 = 0;
        let mut lines_removed: u64 = 0;
        let mut model_agg: HashMap<String, (u64, u64, u64, u64, f64)> = HashMap::new();

        for change_hash in &new_change_hashes {
            let change = match repo.load_change(change_hash) {
                Ok(c) => c,
                Err(e) => {
                    log::debug!(
                        "Could not load change {} for attestation enrichment: {}",
                        change_hash.to_base32(),
                        e,
                    );
                    continue;
                }
            };

            // Aggregate provenance (model, tokens, cost) from each change
            for prov in change.provenance() {
                total_cost += prov.cost.usd;

                let entry = model_agg
                    .entry(prov.model.clone())
                    .or_insert((0, 0, 0, 0, 0.0));
                entry.0 += prov.tokens.input_tokens;
                entry.1 += prov.tokens.output_tokens;
                entry.2 += prov.tokens.cache_read_tokens;
                entry.3 += prov.tokens.cache_write_tokens;
                entry.4 += prov.cost.usd;
            }

            // Count lines from file operations (CRDT semantic layer)
            for file_op in change.file_ops() {
                for line_op in file_op.line_ops() {
                    if line_op.is_insert() {
                        lines_added += 1;
                    } else if line_op.is_delete() {
                        lines_removed += 1;
                    }
                }
            }
        }

        // Build model usage entries from aggregated provenance data
        let models: Vec<ModelUsage> = model_agg
            .into_iter()
            .map(|(model, (inp, out, cr, cw, cost))| {
                ModelUsage::new(&model)
                    .with_input(inp)
                    .with_output(out)
                    .with_cache_read(cr)
                    .with_cache_write(cw)
                    .with_cost(cost)
            })
            .collect();

        // If no provenance data was found in the changes but we know the
        // model from the session, create a minimal model entry so the
        // attestation at least names the model.
        let models = if models.is_empty() && !session.model.is_empty() {
            vec![ModelUsage::new(&session.model)]
        } else {
            models
        };

        // Use the session's vendor (set from OpenCode's provider field)
        // instead of inferring from the agent name, since the session
        // has more accurate data from the actual provider used.
        let vendor = if session.agent_vendor.is_empty() {
            vendor_from_agent_name(&session.agent_name)
        } else {
            &session.agent_vendor
        };
        let agent = AttestAgent::new(&session.agent_name, &session.agent_display_name, vendor);

        let mut builder = Attestation::builder(&session.session_id, agent)
            .cost_usd(total_cost)
            .duration_api_ms(total_api_ms)
            .duration_wall_ms(wall_duration_ms)
            .code_changes(CodeChangeStats::new(lines_added, lines_removed))
            .models(models)
            .changes_covered(new_change_hashes.clone());

        // Chain to previous attestation if this is a resumed session
        if let Some(prev_hash) = previous_attestation {
            builder = builder.previous_attestation(prev_hash);
            builder = builder.notes(format!(
                "Resumed session ({} new changes, {} total in session)",
                new_change_hashes.len(),
                all_change_hashes.len(),
            ));
            log::info!(
                "Chaining to previous attestation {} for resumed session {}",
                prev_hash.to_base32(),
                session.session_id,
            );
        } else if session.turn_count > 0 {
            builder = builder.notes(format!(
                "Auto-created at session end ({} turns)",
                session.turn_count,
            ));
        }

        let attestation = builder.build();

        // Save to the graph
        match repo.save_attestation(&attestation) {
            Ok(hash) => {
                log::info!(
                    "Created attestation {} for session {} ({}{} changes, {} models, +{} -{}, wall: {})",
                    hash.to_base32(),
                    session.session_id,
                    if is_resume { "new: " } else { "" },
                    new_change_hashes.len(),
                    attestation.models.len(),
                    attestation.code_changes.lines_added,
                    attestation.code_changes.lines_removed,
                    attestation.wall_duration_display(),
                );
            }
            Err(e) => {
                log::warn!(
                    "Failed to save attestation for session {}: {}",
                    session.session_id,
                    e,
                );
            }
        }
    }
}

fn vendor_from_agent_name(agent_name: &str) -> &'static str {
    match agent_name {
        "claude-code" => "anthropic",
        "gemini-cli" => "google",
        "codex" => "openai",
        "opencode" => "openai",
        _ => "unknown",
    }
}

impl std::fmt::Debug for TurnOrchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnOrchestrator")
            .field("repo_root", &self.repo_root)
            .field("session_store", &self.session_store)
            .field("watcher_active", &self.watcher.is_active())
            .finish()
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnChanges;
    use crate::watcher::fallback::FallbackWatcher;
    use std::fs;
    use tempfile::TempDir;

    /// Create a test orchestrator with a real FallbackWatcher.
    fn make_orchestrator(dir: &TempDir) -> TurnOrchestrator {
        // Create .atomic directory structure
        fs::create_dir_all(dir.path().join(".atomic/sessions")).unwrap();

        let session_store = SessionStore::for_repo(dir.path()).unwrap();
        let watcher_config = WatcherConfig::new(dir.path());
        let watcher = FallbackWatcher::new(watcher_config);

        TurnOrchestrator::with_watcher(dir.path(), session_store, Box::new(watcher))
    }

    fn session_start_event(session_id: &str) -> TurnEvent {
        TurnEvent::new(session_id, HookType::SessionStart)
    }

    fn turn_start_event(session_id: &str, prompt: &str) -> TurnEvent {
        TurnEvent::new(session_id, HookType::TurnStart).with_prompt(prompt)
    }

    fn turn_end_event(session_id: &str) -> TurnEvent {
        TurnEvent::new(session_id, HookType::TurnEnd)
    }

    fn session_end_event(session_id: &str) -> TurnEvent {
        TurnEvent::new(session_id, HookType::SessionEnd)
    }

    // DispatchResult tests

    #[test]
    fn test_dispatch_result_new() {
        let result = DispatchResult::new("sess-1", Phase::Idle);
        assert_eq!(result.session_id, "sess-1");
        assert_eq!(result.new_phase, Phase::Idle);
        assert!(!result.was_recorded());
        assert!(result.message.is_none());
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn test_dispatch_result_with_message() {
        let result = DispatchResult::new("sess-1", Phase::Idle).with_message("Tracking started");
        assert_eq!(result.message.as_deref(), Some("Tracking started"));
    }

    #[test]
    fn test_dispatch_result_with_warning() {
        let result = DispatchResult::new("sess-1", Phase::Active).with_warning("Stale session");
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.warnings[0], "Stale session");
    }

    #[test]
    fn test_dispatch_result_display() {
        let result = DispatchResult::new("sess-1", Phase::Idle);
        let s = result.to_string();
        assert!(s.contains("sess-1"));
        assert!(s.contains("idle"));
    }

    #[test]
    fn test_dispatch_result_display_with_warning() {
        let result = DispatchResult::new("sess-1", Phase::Active).with_warning("test warning");
        let s = result.to_string();
        assert!(s.contains("⚠"));
        assert!(s.contains("test warning"));
    }

    // SessionStart tests

    #[tokio::test]
    async fn test_session_start_creates_session() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        let result = orch
            .dispatch(session_start_event("sess-new"))
            .await
            .unwrap();

        assert_eq!(result.session_id, "sess-new");
        assert_eq!(result.new_phase, Phase::Idle);
        assert!(result.message.is_some());
        assert!(result.message.unwrap().contains("Atomic is tracking"));

        // Session should be persisted
        let session = orch.session_store.load("sess-new").unwrap();
        assert!(session.is_some());
        assert_eq!(session.unwrap().phase, Phase::Idle);
    }

    #[tokio::test]
    async fn test_session_start_resumes_ended_session() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        // Create and end a session
        let mut session = AgentSession::new("sess-resume", "claude-code", "Claude Code");
        session.phase = Phase::Ended;
        session.turn_count = 3;
        session.ended_at = Some(chrono::Utc::now());
        orch.session_store.save(&session).unwrap();

        // Resume it
        let result = orch
            .dispatch(session_start_event("sess-resume"))
            .await
            .unwrap();

        assert_eq!(result.new_phase, Phase::Idle);
        let msg = result.message.as_deref().unwrap_or("");
        assert!(msg.contains("resumed"));

        // Verify session state
        let session = orch.session_store.load("sess-resume").unwrap().unwrap();
        assert_eq!(session.phase, Phase::Idle);
        assert!(session.ended_at.is_none()); // cleared
        assert_eq!(session.turn_count, 3); // preserved
    }

    // TurnStart tests

    #[tokio::test]
    async fn test_turn_start_transitions_to_active() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        // Create a session first
        orch.dispatch(session_start_event("sess-1")).await.unwrap();

        // Start a turn
        let result = orch
            .dispatch(turn_start_event("sess-1", "Fix the bug"))
            .await
            .unwrap();

        assert_eq!(result.new_phase, Phase::Active);

        // Watcher should be active
        assert!(orch.watcher.is_active());

        // Session should have the prompt
        let session = orch.session_store.load("sess-1").unwrap().unwrap();
        assert_eq!(session.first_prompt.as_deref(), Some("Fix the bug"));
        assert!(session.current_turn_started_at.is_some());
    }

    #[tokio::test]
    async fn test_turn_start_creates_session_if_missing() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        // Directly start a turn without SessionStart
        let result = orch
            .dispatch(turn_start_event("sess-orphan", "Do something"))
            .await
            .unwrap();

        assert_eq!(result.new_phase, Phase::Active);

        // Session should be auto-created
        let session = orch.session_store.load("sess-orphan").unwrap();
        assert!(session.is_some());
    }

    // TurnEnd tests

    #[tokio::test]
    async fn test_turn_end_with_no_changes() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        // Start session and turn
        orch.dispatch(session_start_event("sess-1")).await.unwrap();
        orch.dispatch(turn_start_event("sess-1", "Do nothing"))
            .await
            .unwrap();

        // End turn without modifying any files
        let result = orch.dispatch(turn_end_event("sess-1")).await.unwrap();

        assert_eq!(result.new_phase, Phase::Idle);
        assert!(!result.was_recorded()); // No changes → no recording

        // Turn count should be incremented
        let session = orch.session_store.load("sess-1").unwrap().unwrap();
        assert_eq!(session.turn_count, 1);
    }

    #[tokio::test]
    async fn test_turn_end_with_file_changes() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        // Start session and turn
        orch.dispatch(session_start_event("sess-1")).await.unwrap();
        orch.dispatch(turn_start_event("sess-1", "Add a file"))
            .await
            .unwrap();

        // Create a file during the turn
        fs::write(dir.path().join("new_file.rs"), "fn hello() {}").unwrap();

        // End the turn — recording will fail because there's no initialized
        // Atomic repository (no pristine database), but the session state
        // transitions should still work correctly.
        let result = orch.dispatch(turn_end_event("sess-1")).await.unwrap();

        assert_eq!(result.new_phase, Phase::Idle);

        // Turn count should be incremented even though recording failed
        let session = orch.session_store.load("sess-1").unwrap().unwrap();
        assert_eq!(session.turn_count, 1);

        // Recording failure is reported as a warning, not a fatal error
        // (files_touched is only populated on successful recording)
        assert!(
            !result.warnings.is_empty() || !result.was_recorded(),
            "Expected either warnings about recording failure or no recording"
        );
    }

    // SessionEnd tests

    #[tokio::test]
    async fn test_session_end_transitions_to_ended() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        orch.dispatch(session_start_event("sess-1")).await.unwrap();

        let result = orch.dispatch(session_end_event("sess-1")).await.unwrap();

        assert_eq!(result.new_phase, Phase::Ended);

        let session = orch.session_store.load("sess-1").unwrap().unwrap();
        assert!(session.is_ended());
        assert!(session.ended_at.is_some());
    }

    #[tokio::test]
    async fn test_session_end_unknown_session_is_ok() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        // End a session that doesn't exist — should not error
        let result = orch
            .dispatch(session_end_event("nonexistent"))
            .await
            .unwrap();

        assert_eq!(result.new_phase, Phase::Ended);
    }

    #[tokio::test]
    async fn test_session_end_cancels_active_turn() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        orch.dispatch(session_start_event("sess-1")).await.unwrap();
        orch.dispatch(turn_start_event("sess-1", "Working..."))
            .await
            .unwrap();

        // Watcher should be active
        assert!(orch.watcher.is_active());

        // End the session while turn is active
        let result = orch.dispatch(session_end_event("sess-1")).await.unwrap();

        assert_eq!(result.new_phase, Phase::Ended);
        // Watcher should be cancelled
        assert!(!orch.watcher.is_active());
    }

    // ToolUse tests

    #[tokio::test]
    async fn test_tool_use_event_handled() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        orch.dispatch(session_start_event("sess-1")).await.unwrap();
        orch.dispatch(turn_start_event("sess-1", "Working..."))
            .await
            .unwrap();

        let event = TurnEvent::new("sess-1", HookType::PostToolUse)
            .with_tool_name("Edit")
            .with_tool_use_id("tu-001");

        let result = orch.dispatch(event).await.unwrap();
        assert_eq!(result.new_phase, Phase::Active);
    }

    #[tokio::test]
    async fn test_tool_use_unknown_session_is_ok() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        let event = TurnEvent::new("nonexistent", HookType::PreToolUse).with_tool_name("Task");

        let result = orch.dispatch(event).await.unwrap();
        // Should not error — just returns Idle
        assert_eq!(result.new_phase, Phase::Idle);
    }

    // Full lifecycle integration test

    #[tokio::test]
    async fn test_full_lifecycle_multi_turn() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        // Session starts
        let r = orch.dispatch(session_start_event("sess-lc")).await.unwrap();
        assert_eq!(r.new_phase, Phase::Idle);

        // Turn 1
        let r = orch
            .dispatch(turn_start_event("sess-lc", "First prompt"))
            .await
            .unwrap();
        assert_eq!(r.new_phase, Phase::Active);

        fs::write(dir.path().join("file1.rs"), "fn one() {}").unwrap();

        let r = orch.dispatch(turn_end_event("sess-lc")).await.unwrap();
        assert_eq!(r.new_phase, Phase::Idle);

        // Turn 2
        let r = orch
            .dispatch(turn_start_event("sess-lc", "Second prompt"))
            .await
            .unwrap();
        assert_eq!(r.new_phase, Phase::Active);

        fs::write(dir.path().join("file2.rs"), "fn two() {}").unwrap();

        let r = orch.dispatch(turn_end_event("sess-lc")).await.unwrap();
        assert_eq!(r.new_phase, Phase::Idle);

        // Session ends
        let r = orch.dispatch(session_end_event("sess-lc")).await.unwrap();
        assert_eq!(r.new_phase, Phase::Ended);

        // Verify final session state — phase transitions and turn counting
        // work correctly even without a real Atomic repository for recording.
        // files_touched is only populated on successful recordings, which
        // require a real initialized repo (tested in integration tests).
        let session = orch.session_store.load("sess-lc").unwrap().unwrap();
        assert_eq!(session.turn_count, 2);
        assert!(session.is_ended());
        assert_eq!(session.first_prompt.as_deref(), Some("First prompt"));
    }

    #[tokio::test]
    async fn test_lifecycle_ctrl_c_recovery() {
        let dir = TempDir::new().unwrap();
        let mut orch = make_orchestrator(&dir);

        orch.dispatch(session_start_event("sess-cr")).await.unwrap();

        // Start a turn
        orch.dispatch(turn_start_event("sess-cr", "First attempt"))
            .await
            .unwrap();

        // Agent crashes — user restarts with a new prompt
        // This sends another TurnStart without a TurnEnd
        let r = orch
            .dispatch(turn_start_event("sess-cr", "Second attempt"))
            .await
            .unwrap();

        // Should still be Active (Ctrl-C recovery)
        assert_eq!(r.new_phase, Phase::Active);

        // End the second turn normally
        let r = orch.dispatch(turn_end_event("sess-cr")).await.unwrap();
        assert_eq!(r.new_phase, Phase::Idle);

        let session = orch.session_store.load("sess-cr").unwrap().unwrap();
        // First prompt is preserved (not overwritten by second)
        assert_eq!(session.first_prompt.as_deref(), Some("First attempt"));
    }

    // Debug trait

    #[test]
    fn test_orchestrator_debug() {
        let dir = TempDir::new().unwrap();
        let orch = make_orchestrator(&dir);
        let debug = format!("{:?}", orch);
        assert!(debug.contains("TurnOrchestrator"));
        assert!(debug.contains("repo_root"));
    }
}
