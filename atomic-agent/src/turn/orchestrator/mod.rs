//! Turn orchestrator — dispatches events through state machine and recording.
//!
//! The `TurnOrchestrator` is the central coordinator that connects:
//!
//! - **Agent hooks** (via [`TurnEvent`]) — lifecycle events from the agent
//! - **State machine** (via [`crate::turn::phase::transition`]) — determines what actions to take
//! - **File watcher** (via [`FileWatcher`]) — optional real-time file tracking
//! - **Recording** (via [`record_turn`](crate::record::record_turn)) — status → add → record workflow
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
//!     ├─ SessionStart → create/resume session, create view
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

mod attestation;
mod provenance;
mod session_end;
mod session_start;
mod turn;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::error::AgentResult;
use crate::event::{HookType, TurnEvent};
use crate::record::TurnRecordOutcome;
use crate::turn::phase::Phase;
use crate::turn::session::SessionStore;
use crate::watcher::{self, FileWatcher, WatcherConfig};

// ═══════════════════════════════════════════════════════════════════════
// DispatchResult
// ═══════════════════════════════════════════════════════════════════════

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

    /// The Atomic view the session records onto (the per-session agent view).
    ///
    /// Set on turn events so a caller can locate the recorded change, which
    /// lives on this view rather than the default/shared view. `None` when the
    /// session view is not known for this event.
    pub view: Option<String>,

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
            view: None,
            message: None,
            warnings: Vec::new(),
        }
    }

    /// Set the recorded change outcome.
    pub(crate) fn with_change(mut self, outcome: TurnRecordOutcome) -> Self {
        self.change_recorded = Some(outcome);
        self
    }

    /// Set the session's record view.
    pub(crate) fn with_view(mut self, view: impl Into<String>) -> Self {
        self.view = Some(view.into());
        self
    }

    /// Set the user-facing message.
    pub(crate) fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Add a warning.
    pub(crate) fn with_warning(mut self, warning: impl Into<String>) -> Self {
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

// ═══════════════════════════════════════════════════════════════════════
// TurnOrchestrator
// ═══════════════════════════════════════════════════════════════════════

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
    pub(crate) repo_root: PathBuf,

    /// Persistent session state storage.
    pub(crate) session_store: SessionStore,

    /// File change detection backend.
    pub(crate) watcher: Box<dyn FileWatcher>,

    /// Agent registry key (e.g., "claude-code", "gemini-cli").
    ///
    /// Set by the CLI hook handler so that sessions created by this
    /// orchestrator get the correct agent name instead of "unknown".
    pub(crate) agent_name: String,

    /// Human-readable agent display name (e.g., "Claude Code").
    pub(crate) agent_display_name: String,
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

    // ── Main dispatch ──────────────────────────────────────────────

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
}

// ═══════════════════════════════════════════════════════════════════════
// Free functions
// ═══════════════════════════════════════════════════════════════════════

/// Infer vendor from the agent registry name.
///
/// The model comes from the agent's hook payload (e.g., SessionStart sends
/// `"model": "claude-sonnet-4-5-20250929"`). We don't guess the model —
/// only the vendor, which is deterministic from the agent name.
pub(crate) fn vendor_from_agent_name(agent_name: &str) -> &'static str {
    match agent_name {
        "claude-code" => "anthropic",
        "cline" => "unknown", // Cline supports multiple providers; real vendor comes from hook payload
        "copilot" => "github",
        "cursor" => "anthropic",
        "gemini-cli" => "google",
        "codex" => "openai",
        "kiro" => "amazon-bedrock",
        "opencode" => "openai",
        _ => "unknown",
    }
}

/// Truncate a string to `max_len` bytes, breaking at a word boundary where
/// possible and appending `"..."`.  Mirrors the same helper in sibling modules.
pub(crate) fn truncate_prompt(prompt: &str, max_len: usize) -> String {
    let trimmed = prompt.trim();
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }

    // Try to break at a word boundary in the first `max_len - 3` bytes.
    let truncated = &trimmed[..max_len.saturating_sub(3)];
    if let Some(last_space) = truncated.rfind(' ') {
        if last_space > max_len / 2 {
            return format!("{}...", &truncated[..last_space]);
        }
    }

    format!("{}...", truncated)
}

impl TurnOrchestrator {
    /// Fast check for working copy changes.
    ///
    /// Uses `StatusOptions::fast()` so turn-end recording sees both tracked
    /// changes and untracked files created by the agent. This intentionally
    /// includes the untracked filesystem walk: if we ignore untracked files
    /// here, a turn that only creates new files exits before `record_turn()`
    /// can auto-add and record them with provenance.
    pub(crate) fn has_working_copy_changes(&self) -> bool {
        let repo = match atomic_repository::Repository::open_readonly(&self.repo_root) {
            Ok(r) => r,
            Err(_) => return true, // can't check — assume changes
        };

        let opts = atomic_repository::status::StatusOptions::fast().with_untracked(true);
        match repo.status(opts) {
            Ok(status) => !status.is_clean() || status.has_untracked(),
            Err(_) => true, // can't check — assume changes
        }
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
