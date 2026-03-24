//! Turn lifecycle state machine.
//!
//! This module implements the phase/event/action model for agent turn tracking.
//! It is a direct port of Entire CLI's `session/phase.go` state machine, but
//! with actions mapped to Atomic operations instead of git shadow branch
//! operations.
//!
//! # Key Differences from Entire CLI
//!
//! | Entire CLI | Atomic Agent | Why |
//! |---|---|---|
//! | `ActionCondense` | `RecordTurn` | Atomic records changes, not git condense |
//! | `ActionMigrateShadowBranch` | **Deleted** | No shadow branches in Atomic |
//! | `IsRebaseInProgress` context | **Deleted** | Atomic doesn't have rebase |
//! | `PhaseActiveCommitted` | `ActiveRecorded` | We record, not commit |
//!
//! # State Machine
//!
//! The state machine is a pure function: `transition(phase, event, context) -> result`.
//! It has no side effects. The caller (orchestrator) is responsible for executing
//! the actions returned in the `TransitionResult`.
//!
//! ```text
//!                    TurnStart              TurnEnd + RecordTurn
//!          ┌───────────────────┐  ┌──────────────────────────────┐
//!          │                   │  │                               │
//!          ▼                   │  ▼                               │
//!     ┌─────────┐        ┌─────────┐                       ┌─────────┐
//!     │  Idle   │───────▶│ Active  │──────────────────────▶│  Idle   │
//!     └─────────┘        └─────────┘                       └─────────┘
//!          │                   │
//!          │ SessionStop       │ Recorded (mid-turn)
//!          ▼                   ▼
//!     ┌─────────┐        ┌───────────────┐
//!     │  Ended  │        │ActiveRecorded │──TurnEnd + RecordTurn──▶ Idle
//!     └─────────┘        └───────────────┘
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::turn::phase::{Phase, Event, Action, TransitionContext, transition};
//!
//! // Agent turn starts
//! let result = transition(Phase::Idle, Event::TurnStart, TransitionContext::default());
//! assert_eq!(result.new_phase, Phase::Active);
//! assert!(result.actions.contains(&Action::UpdateInteraction));
//!
//! // Agent turn ends — should record the turn
//! let result = transition(Phase::Active, Event::TurnEnd, TransitionContext::default());
//! assert_eq!(result.new_phase, Phase::Idle);
//! assert!(result.actions.contains(&Action::RecordTurn));
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};

// Phase

/// The lifecycle phase of an agent session.
///
/// Sessions move through phases as agent turns start, end, and as changes
/// are recorded. The phase determines what actions are valid and what
/// transitions are possible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum Phase {
    /// No active turn. The session is waiting for the next prompt.
    ///
    /// This is the initial phase and the phase after a turn completes.
    /// Recording is possible (e.g., user manually records between turns).
    #[default]
    Idle,

    /// An agent turn is in progress (between TurnStart and TurnEnd).
    ///
    /// The agent is working on a prompt. File changes are being tracked
    /// by the watcher. Recording should be deferred until the turn ends.
    Active,

    /// An agent turn is in progress AND a record happened mid-turn.
    ///
    /// This occurs when the user (or another process) records changes while
    /// the agent is still working. The turn's changes will be recorded
    /// again when the turn ends.
    ///
    /// Named `ActiveRecorded` instead of Entire CLI's `ActiveCommitted`
    /// because Atomic records changes, not git commits.
    ActiveRecorded,

    /// The session has ended (agent process exited).
    ///
    /// The session can be re-entered if the agent starts again with the
    /// same session ID (transitions back to Idle via SessionStart).
    Ended,
}

impl Phase {
    /// Returns `true` if the phase represents an active agent turn.
    ///
    /// An active turn means the agent is currently working on a prompt
    /// and file changes should be tracked.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_agent::turn::phase::Phase;
    ///
    /// assert!(Phase::Active.is_active());
    /// assert!(Phase::ActiveRecorded.is_active());
    /// assert!(!Phase::Idle.is_active());
    /// assert!(!Phase::Ended.is_active());
    /// ```
    pub fn is_active(&self) -> bool {
        matches!(self, Phase::Active | Phase::ActiveRecorded)
    }

    /// Returns `true` if the session has ended.
    pub fn is_ended(&self) -> bool {
        matches!(self, Phase::Ended)
    }

    /// Returns `true` if the session is idle (no active turn).
    pub fn is_idle(&self) -> bool {
        matches!(self, Phase::Idle)
    }

    /// Normalize a string to a Phase, defaulting to `Idle` for unknown values.
    ///
    /// This provides backward compatibility with session state files that
    /// may have empty or unrecognized phase values.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_agent::turn::phase::Phase;
    ///
    /// assert_eq!(Phase::from_str_normalized("active"), Phase::Active);
    /// assert_eq!(Phase::from_str_normalized(""), Phase::Idle);
    /// assert_eq!(Phase::from_str_normalized("garbage"), Phase::Idle);
    /// ```
    pub fn from_str_normalized(s: &str) -> Self {
        match s {
            "idle" => Phase::Idle,
            "active" => Phase::Active,
            "active_recorded" => Phase::ActiveRecorded,
            "ended" => Phase::Ended,
            _ => Phase::Idle,
        }
    }

    /// Returns the canonical string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Active => "active",
            Phase::ActiveRecorded => "active_recorded",
            Phase::Ended => "ended",
        }
    }
}


impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// Event

/// An event that triggers a state machine transition.
///
/// Events are generated by the orchestrator in response to agent hook
/// callbacks and other lifecycle events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    /// The agent began working on a prompt (UserPromptSubmit / TurnStart hook).
    ///
    /// This signals the watcher to capture the filesystem baseline.
    TurnStart,

    /// The agent finished its turn (Stop / TurnEnd hook).
    ///
    /// This signals the watcher to query for changes and potentially
    /// record an Atomic change.
    TurnEnd,

    /// A change was recorded (either by the user or programmatically).
    ///
    /// This replaces Entire CLI's `EventGitCommit`. In Atomic, we record
    /// changes, not git commits.
    Recorded,

    /// The agent session process started (SessionStart hook).
    ///
    /// This may re-enter an ended session or start a new one.
    SessionStart,

    /// The agent session process stopped (SessionEnd / SessionStop hook).
    ///
    /// The session transitions to Ended. It can be re-entered later
    /// if the agent starts again with the same session ID.
    SessionStop,
}

impl Event {
    /// Returns the canonical string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Event::TurnStart => "turn_start",
            Event::TurnEnd => "turn_end",
            Event::Recorded => "recorded",
            Event::SessionStart => "session_start",
            Event::SessionStop => "session_stop",
        }
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::TurnStart => write!(f, "TurnStart"),
            Event::TurnEnd => write!(f, "TurnEnd"),
            Event::Recorded => write!(f, "Recorded"),
            Event::SessionStart => write!(f, "SessionStart"),
            Event::SessionStop => write!(f, "SessionStop"),
        }
    }
}

// Action

/// An action that the orchestrator should execute after a transition.
///
/// The state machine declares actions but does not execute them. The
/// orchestrator is responsible for dispatching each action to the
/// appropriate subsystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Record the turn's file changes as an Atomic change.
    ///
    /// Replaces Entire CLI's `ActionCondense`. The orchestrator should:
    /// 1. Call `watcher.end_turn()` to get `TurnChanges`
    /// 2. Call `record_turn()` to create an Atomic change
    /// 3. Apply the change to the agent's stack
    RecordTurn,

    /// Record only if files were actually modified during the turn.
    ///
    /// Replaces Entire CLI's `ActionCondenseIfFilesTouched`. Used in the
    /// `Ended` phase when a record event occurs — only creates a change
    /// if there are actual file modifications to capture.
    RecordIfChanged,

    /// Discard the session if no files were touched.
    ///
    /// Same as Entire CLI's `ActionDiscardIfNoFiles`. If the session ended
    /// and a record event occurs with no file changes, the session state
    /// can be cleaned up.
    DiscardIfNoFiles,

    /// Update the session's `last_interaction` timestamp.
    ///
    /// Applied directly to the session state by `apply_common_actions()`.
    /// This is used for stale session detection in `atomic agent status`.
    UpdateInteraction,

    /// Clear the `ended_at` timestamp (session is re-entering).
    ///
    /// Applied directly to the session state by `apply_common_actions()`.
    /// This happens when a SessionStart event arrives for an ended session.
    ClearEndedAt,

    /// Warn the user about stale concurrent sessions.
    ///
    /// This is emitted when a SessionStart or TurnStart event arrives
    /// while another session is already active. The orchestrator should
    /// log a warning but continue processing.
    WarnStaleSession,
}

impl Action {
    /// Returns `true` if this action requires strategy-specific handling.
    ///
    /// "Common" actions (`UpdateInteraction`, `ClearEndedAt`) are applied
    /// directly to the session state by `apply_common_actions()`.
    /// Strategy-specific actions are returned to the caller for dispatch.
    pub fn is_strategy_specific(&self) -> bool {
        matches!(
            self,
            Action::RecordTurn
                | Action::RecordIfChanged
                | Action::DiscardIfNoFiles
                | Action::WarnStaleSession
        )
    }

    /// Returns the canonical string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::RecordTurn => "record_turn",
            Action::RecordIfChanged => "record_if_changed",
            Action::DiscardIfNoFiles => "discard_if_no_files",
            Action::UpdateInteraction => "update_interaction",
            Action::ClearEndedAt => "clear_ended_at",
            Action::WarnStaleSession => "warn_stale_session",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// TransitionContext

/// Read-only context for transitions that need to inspect session state.
///
/// Some transitions depend on session state (e.g., whether any files were
/// touched). This struct provides that context without coupling the state
/// machine to the session type.
///
/// # Default
///
/// The default context has `has_files_changed: false`, which is the
/// conservative assumption for most transitions.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransitionContext {
    /// Whether any files were modified during the session.
    ///
    /// This only affects the `Ended + Recorded` transition:
    /// - `true` → `RecordIfChanged` action
    /// - `false` → `DiscardIfNoFiles` action
    pub has_files_changed: bool,
}

impl TransitionContext {
    /// Create a context indicating files were changed.
    pub fn with_files_changed() -> Self {
        Self {
            has_files_changed: true,
        }
    }
}

// TransitionResult

/// The outcome of a state machine transition.
///
/// Contains the new phase and a list of actions the orchestrator should
/// execute. Actions are ordered — the orchestrator should execute them
/// in sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionResult {
    /// The phase after the transition.
    pub new_phase: Phase,

    /// Actions to execute after the transition.
    ///
    /// May be empty for no-op transitions (e.g., `Idle + SessionStart`).
    pub actions: Vec<Action>,
}

impl TransitionResult {
    /// Create a result with no actions.
    fn noop(phase: Phase) -> Self {
        Self {
            new_phase: phase,
            actions: Vec::new(),
        }
    }

    /// Create a result with the given actions.
    fn with_actions(phase: Phase, actions: Vec<Action>) -> Self {
        Self {
            new_phase: phase,
            actions,
        }
    }

    /// Returns `true` if this transition has no actions.
    pub fn is_noop(&self) -> bool {
        self.actions.is_empty()
    }

    /// Returns `true` if this transition requires recording.
    pub fn requires_recording(&self) -> bool {
        self.actions
            .iter()
            .any(|a| matches!(a, Action::RecordTurn | Action::RecordIfChanged))
    }

    /// Returns the strategy-specific actions (filtering out common actions).
    pub fn strategy_actions(&self) -> Vec<Action> {
        self.actions
            .iter()
            .filter(|a| a.is_strategy_specific())
            .copied()
            .collect()
    }
}

impl fmt::Display for TransitionResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.new_phase)?;
        if !self.actions.is_empty() {
            let action_strs: Vec<&str> = self.actions.iter().map(|a| a.as_str()).collect();
            write!(f, " [{}]", action_strs.join(", "))?;
        }
        Ok(())
    }
}

// Transition Function

/// Compute the next phase and required actions given the current phase
/// and an event.
///
/// This is a **pure function** with no side effects. The caller (orchestrator)
/// is responsible for executing the returned actions.
///
/// Unknown or empty phase values are normalized to `Idle` for backward
/// compatibility with session state files created before phase tracking.
///
/// # Arguments
///
/// * `current` — The current session phase
/// * `event` — The event that occurred
/// * `ctx` — Read-only context about the session state
///
/// # Returns
///
/// A `TransitionResult` containing the new phase and actions to execute.
///
/// # Example
///
/// ```rust
/// use atomic_agent::turn::phase::*;
///
/// // Full turn lifecycle
/// let r = transition(Phase::Idle, Event::TurnStart, TransitionContext::default());
/// assert_eq!(r.new_phase, Phase::Active);
///
/// let r = transition(Phase::Active, Event::TurnEnd, TransitionContext::default());
/// assert_eq!(r.new_phase, Phase::Idle);
/// assert!(r.requires_recording());
/// ```
pub fn transition(current: Phase, event: Event, ctx: TransitionContext) -> TransitionResult {
    match current {
        Phase::Idle => transition_from_idle(event),
        Phase::Active => transition_from_active(event),
        Phase::ActiveRecorded => transition_from_active_recorded(event),
        Phase::Ended => transition_from_ended(event, ctx),
    }
}

fn transition_from_idle(event: Event) -> TransitionResult {
    match event {
        // Agent starts a turn → go active
        Event::TurnStart => {
            TransitionResult::with_actions(Phase::Active, vec![Action::UpdateInteraction])
        }

        // Turn end while idle → still attempt to record.
        // OpenCode's plugin fires session.idle (TurnEnd) without a preceding
        // TurnStart when the bus event ordering doesn't guarantee it.
        // Rather than silently dropping the turn, attempt a record — if
        // the working copy is clean, record_turn returns EmptyTurn harmlessly.
        Event::TurnEnd => TransitionResult::with_actions(
            Phase::Idle,
            vec![Action::RecordIfChanged, Action::UpdateInteraction],
        ),

        // A change was recorded while idle (user manually recorded) → record
        Event::Recorded => TransitionResult::with_actions(
            Phase::Idle,
            vec![Action::RecordTurn, Action::UpdateInteraction],
        ),

        // Session started while idle → no-op (already condensable)
        Event::SessionStart => TransitionResult::noop(Phase::Idle),

        // Session stopped while idle → end
        Event::SessionStop => {
            TransitionResult::with_actions(Phase::Ended, vec![Action::UpdateInteraction])
        }
    }
}

fn transition_from_active(event: Event) -> TransitionResult {
    match event {
        // Turn start while active → Ctrl-C recovery, continue
        // The previous turn was interrupted — just continue with the new one.
        // Any pending changes from the interrupted turn will be captured
        // when this new turn ends.
        Event::TurnStart => {
            TransitionResult::with_actions(Phase::Active, vec![Action::UpdateInteraction])
        }

        // Turn ended → go idle, record the turn
        Event::TurnEnd => TransitionResult::with_actions(
            Phase::Idle,
            vec![Action::RecordTurn, Action::UpdateInteraction],
        ),

        // A record happened mid-turn → note it, defer turn recording to TurnEnd
        Event::Recorded => {
            TransitionResult::with_actions(Phase::ActiveRecorded, vec![Action::UpdateInteraction])
        }

        // Another session started while this one is active → warn
        Event::SessionStart => {
            TransitionResult::with_actions(Phase::Active, vec![Action::WarnStaleSession])
        }

        // Session stopped while active → end
        Event::SessionStop => {
            TransitionResult::with_actions(Phase::Ended, vec![Action::UpdateInteraction])
        }
    }
}

fn transition_from_active_recorded(event: Event) -> TransitionResult {
    match event {
        // Turn start while active+recorded → Ctrl-C recovery
        // Same as Active + TurnStart: previous turn was interrupted after
        // a mid-turn record. Transition back to Active for the new turn.
        Event::TurnStart => {
            TransitionResult::with_actions(Phase::Active, vec![Action::UpdateInteraction])
        }

        // Turn ended after mid-turn record → record the turn
        Event::TurnEnd => TransitionResult::with_actions(
            Phase::Idle,
            vec![Action::RecordTurn, Action::UpdateInteraction],
        ),

        // Another record while already in active+recorded → stay, update
        Event::Recorded => {
            TransitionResult::with_actions(Phase::ActiveRecorded, vec![Action::UpdateInteraction])
        }

        // Another session started → warn
        Event::SessionStart => {
            TransitionResult::with_actions(Phase::ActiveRecorded, vec![Action::WarnStaleSession])
        }

        // Session stopped → end
        Event::SessionStop => {
            TransitionResult::with_actions(Phase::Ended, vec![Action::UpdateInteraction])
        }
    }
}

fn transition_from_ended(event: Event, ctx: TransitionContext) -> TransitionResult {
    match event {
        // Turn start on ended session → re-enter
        Event::TurnStart => TransitionResult::with_actions(
            Phase::Active,
            vec![Action::ClearEndedAt, Action::UpdateInteraction],
        ),

        // Turn end while ended → no-op
        Event::TurnEnd => TransitionResult::noop(Phase::Ended),

        // A record on ended session → depends on whether files changed
        Event::Recorded => {
            if ctx.has_files_changed {
                TransitionResult::with_actions(
                    Phase::Ended,
                    vec![Action::RecordIfChanged, Action::UpdateInteraction],
                )
            } else {
                TransitionResult::with_actions(
                    Phase::Ended,
                    vec![Action::DiscardIfNoFiles, Action::UpdateInteraction],
                )
            }
        }

        // Session starting on ended session → re-enter as idle
        Event::SessionStart => {
            TransitionResult::with_actions(Phase::Idle, vec![Action::ClearEndedAt])
        }

        // Session stop on ended session → no-op (already ended)
        Event::SessionStop => TransitionResult::noop(Phase::Ended),
    }
}

// Apply Common Actions

/// Trait for types that can have common actions applied to them.
///
/// This is implemented by `AgentSession` to allow the state machine
/// to update timestamps and phase directly, without the orchestrator
/// needing to handle these common cases.
pub trait SessionState {
    /// Set the current phase.
    fn set_phase(&mut self, phase: Phase);

    /// Update the last interaction timestamp to now.
    fn touch_interaction(&mut self);

    /// Clear the ended_at timestamp (session is re-entering).
    fn clear_ended_at(&mut self);
}

/// Apply the common (non-strategy-specific) actions from a transition result
/// to the given session state.
///
/// This updates `phase`, `last_interaction`, and `ended_at` as indicated
/// by the transition actions.
///
/// Returns the subset of actions that require strategy-specific handling
/// (e.g., `RecordTurn`, `RecordIfChanged`, `WarnStaleSession`). The caller
/// is responsible for dispatching those.
///
/// # Example
///
/// ```rust,ignore
/// let result = transition(Phase::Idle, Event::TurnStart, ctx);
/// let remaining = apply_common_actions(&mut session, &result);
/// // remaining is empty — UpdateInteraction was handled
/// // session.phase is now Active
/// // session.last_interaction was updated
/// ```
pub fn apply_common_actions<S: SessionState>(
    state: &mut S,
    result: &TransitionResult,
) -> Vec<Action> {
    state.set_phase(result.new_phase);

    let mut remaining = Vec::new();

    for action in &result.actions {
        match action {
            Action::UpdateInteraction => {
                state.touch_interaction();
            }
            Action::ClearEndedAt => {
                state.clear_ended_at();
            }
            // Strategy-specific actions are passed through
            Action::RecordTurn
            | Action::RecordIfChanged
            | Action::DiscardIfNoFiles
            | Action::WarnStaleSession => {
                remaining.push(*action);
            }
        }
    }

    remaining
}

// All phases and events for exhaustive testing

/// All possible phases, for exhaustive testing.
#[allow(dead_code)]
const ALL_PHASES: [Phase; 4] = [
    Phase::Idle,
    Phase::Active,
    Phase::ActiveRecorded,
    Phase::Ended,
];

/// All possible events, for exhaustive testing.
#[allow(dead_code)]
const ALL_EVENTS: [Event; 5] = [
    Event::TurnStart,
    Event::TurnEnd,
    Event::Recorded,
    Event::SessionStart,
    Event::SessionStop,
];

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // Phase tests

    #[test]
    fn test_phase_is_active() {
        assert!(Phase::Active.is_active());
        assert!(Phase::ActiveRecorded.is_active());
        assert!(!Phase::Idle.is_active());
        assert!(!Phase::Ended.is_active());
    }

    #[test]
    fn test_phase_is_ended() {
        assert!(Phase::Ended.is_ended());
        assert!(!Phase::Idle.is_ended());
        assert!(!Phase::Active.is_ended());
        assert!(!Phase::ActiveRecorded.is_ended());
    }

    #[test]
    fn test_phase_is_idle() {
        assert!(Phase::Idle.is_idle());
        assert!(!Phase::Active.is_idle());
        assert!(!Phase::ActiveRecorded.is_idle());
        assert!(!Phase::Ended.is_idle());
    }

    #[test]
    fn test_phase_default() {
        assert_eq!(Phase::default(), Phase::Idle);
    }

    #[test]
    fn test_phase_from_str_normalized() {
        assert_eq!(Phase::from_str_normalized("idle"), Phase::Idle);
        assert_eq!(Phase::from_str_normalized("active"), Phase::Active);
        assert_eq!(
            Phase::from_str_normalized("active_recorded"),
            Phase::ActiveRecorded
        );
        assert_eq!(Phase::from_str_normalized("ended"), Phase::Ended);
    }

    #[test]
    fn test_phase_from_str_normalized_unknown() {
        assert_eq!(Phase::from_str_normalized(""), Phase::Idle);
        assert_eq!(Phase::from_str_normalized("garbage"), Phase::Idle);
        assert_eq!(Phase::from_str_normalized("Active"), Phase::Idle);
        assert_eq!(Phase::from_str_normalized("IDLE"), Phase::Idle);
    }

    #[test]
    fn test_phase_as_str_roundtrip() {
        for phase in &ALL_PHASES {
            let s = phase.as_str();
            let recovered = Phase::from_str_normalized(s);
            assert_eq!(
                *phase, recovered,
                "Phase roundtrip failed for {:?} -> {:?} -> {:?}",
                phase, s, recovered
            );
        }
    }

    #[test]
    fn test_phase_display() {
        assert_eq!(Phase::Idle.to_string(), "idle");
        assert_eq!(Phase::Active.to_string(), "active");
        assert_eq!(Phase::ActiveRecorded.to_string(), "active_recorded");
        assert_eq!(Phase::Ended.to_string(), "ended");
    }

    #[test]
    fn test_phase_serde_roundtrip() {
        for phase in &ALL_PHASES {
            let json = serde_json::to_string(phase).unwrap();
            let recovered: Phase = serde_json::from_str(&json).unwrap();
            assert_eq!(*phase, recovered, "Serde roundtrip failed for {:?}", phase);
        }
    }

    #[test]
    fn test_phase_serde_values() {
        assert_eq!(serde_json::to_string(&Phase::Idle).unwrap(), "\"idle\"");
        assert_eq!(serde_json::to_string(&Phase::Active).unwrap(), "\"active\"");
        assert_eq!(
            serde_json::to_string(&Phase::ActiveRecorded).unwrap(),
            "\"active_recorded\""
        );
        assert_eq!(serde_json::to_string(&Phase::Ended).unwrap(), "\"ended\"");
    }

    // Event tests

    #[test]
    fn test_event_as_str() {
        assert_eq!(Event::TurnStart.as_str(), "turn_start");
        assert_eq!(Event::TurnEnd.as_str(), "turn_end");
        assert_eq!(Event::Recorded.as_str(), "recorded");
        assert_eq!(Event::SessionStart.as_str(), "session_start");
        assert_eq!(Event::SessionStop.as_str(), "session_stop");
    }

    #[test]
    fn test_event_display() {
        assert_eq!(Event::TurnStart.to_string(), "TurnStart");
        assert_eq!(Event::TurnEnd.to_string(), "TurnEnd");
        assert_eq!(Event::Recorded.to_string(), "Recorded");
        assert_eq!(Event::SessionStart.to_string(), "SessionStart");
        assert_eq!(Event::SessionStop.to_string(), "SessionStop");
    }

    // Action tests

    #[test]
    fn test_action_is_strategy_specific() {
        assert!(Action::RecordTurn.is_strategy_specific());
        assert!(Action::RecordIfChanged.is_strategy_specific());
        assert!(Action::DiscardIfNoFiles.is_strategy_specific());
        assert!(Action::WarnStaleSession.is_strategy_specific());

        assert!(!Action::UpdateInteraction.is_strategy_specific());
        assert!(!Action::ClearEndedAt.is_strategy_specific());
    }

    #[test]
    fn test_action_display() {
        assert_eq!(Action::RecordTurn.to_string(), "record_turn");
        assert_eq!(Action::RecordIfChanged.to_string(), "record_if_changed");
        assert_eq!(Action::DiscardIfNoFiles.to_string(), "discard_if_no_files");
        assert_eq!(Action::UpdateInteraction.to_string(), "update_interaction");
        assert_eq!(Action::ClearEndedAt.to_string(), "clear_ended_at");
        assert_eq!(Action::WarnStaleSession.to_string(), "warn_stale_session");
    }

    // TransitionResult tests

    #[test]
    fn test_result_noop() {
        let result = TransitionResult::noop(Phase::Idle);
        assert!(result.is_noop());
        assert!(!result.requires_recording());
        assert!(result.strategy_actions().is_empty());
    }

    #[test]
    fn test_result_with_record() {
        let result = TransitionResult::with_actions(
            Phase::Idle,
            vec![Action::RecordTurn, Action::UpdateInteraction],
        );
        assert!(!result.is_noop());
        assert!(result.requires_recording());
        assert_eq!(result.strategy_actions(), vec![Action::RecordTurn]);
    }

    #[test]
    fn test_result_display() {
        let result = TransitionResult::noop(Phase::Idle);
        assert_eq!(result.to_string(), "idle");

        let result = TransitionResult::with_actions(
            Phase::Active,
            vec![Action::UpdateInteraction, Action::WarnStaleSession],
        );
        assert_eq!(
            result.to_string(),
            "active [update_interaction, warn_stale_session]"
        );
    }

    // Transition: from Idle

    #[test]
    fn test_idle_turn_start() {
        let r = transition(Phase::Idle, Event::TurnStart, TransitionContext::default());
        assert_eq!(r.new_phase, Phase::Active);
        assert_eq!(r.actions, vec![Action::UpdateInteraction]);
    }

    #[test]
    fn test_idle_turn_end() {
        // TurnEnd from Idle still attempts recording because OpenCode's
        // plugin can fire session.idle (TurnEnd) without a preceding
        // TurnStart when bus event ordering doesn't guarantee it.
        let r = transition(Phase::Idle, Event::TurnEnd, TransitionContext::default());
        assert_eq!(r.new_phase, Phase::Idle);
        assert_eq!(
            r.actions,
            vec![Action::RecordIfChanged, Action::UpdateInteraction]
        );
    }

    #[test]
    fn test_idle_recorded() {
        let r = transition(Phase::Idle, Event::Recorded, TransitionContext::default());
        assert_eq!(r.new_phase, Phase::Idle);
        assert_eq!(
            r.actions,
            vec![Action::RecordTurn, Action::UpdateInteraction]
        );
    }

    #[test]
    fn test_idle_session_start() {
        let r = transition(
            Phase::Idle,
            Event::SessionStart,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Idle);
        assert!(r.is_noop());
    }

    #[test]
    fn test_idle_session_stop() {
        let r = transition(
            Phase::Idle,
            Event::SessionStop,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Ended);
        assert_eq!(r.actions, vec![Action::UpdateInteraction]);
    }

    // Transition: from Active

    #[test]
    fn test_active_turn_start_ctrl_c_recovery() {
        let r = transition(
            Phase::Active,
            Event::TurnStart,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Active);
        assert_eq!(r.actions, vec![Action::UpdateInteraction]);
    }

    #[test]
    fn test_active_turn_end() {
        let r = transition(Phase::Active, Event::TurnEnd, TransitionContext::default());
        assert_eq!(r.new_phase, Phase::Idle);
        assert_eq!(
            r.actions,
            vec![Action::RecordTurn, Action::UpdateInteraction]
        );
        assert!(r.requires_recording());
    }

    #[test]
    fn test_active_recorded_mid_turn() {
        let r = transition(Phase::Active, Event::Recorded, TransitionContext::default());
        assert_eq!(r.new_phase, Phase::ActiveRecorded);
        assert_eq!(r.actions, vec![Action::UpdateInteraction]);
    }

    #[test]
    fn test_active_session_start_warn() {
        let r = transition(
            Phase::Active,
            Event::SessionStart,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Active);
        assert_eq!(r.actions, vec![Action::WarnStaleSession]);
    }

    #[test]
    fn test_active_session_stop() {
        let r = transition(
            Phase::Active,
            Event::SessionStop,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Ended);
        assert_eq!(r.actions, vec![Action::UpdateInteraction]);
    }

    // Transition: from ActiveRecorded

    #[test]
    fn test_active_recorded_turn_start_ctrl_c_recovery() {
        let r = transition(
            Phase::ActiveRecorded,
            Event::TurnStart,
            TransitionContext::default(),
        );
        // Goes back to Active — the mid-turn record is acknowledged,
        // new turn starts fresh
        assert_eq!(r.new_phase, Phase::Active);
        assert_eq!(r.actions, vec![Action::UpdateInteraction]);
    }

    #[test]
    fn test_active_recorded_turn_end() {
        let r = transition(
            Phase::ActiveRecorded,
            Event::TurnEnd,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Idle);
        assert_eq!(
            r.actions,
            vec![Action::RecordTurn, Action::UpdateInteraction]
        );
        assert!(r.requires_recording());
    }

    #[test]
    fn test_active_recorded_recorded_again() {
        let r = transition(
            Phase::ActiveRecorded,
            Event::Recorded,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::ActiveRecorded);
        assert_eq!(r.actions, vec![Action::UpdateInteraction]);
    }

    #[test]
    fn test_active_recorded_session_start_warn() {
        let r = transition(
            Phase::ActiveRecorded,
            Event::SessionStart,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::ActiveRecorded);
        assert_eq!(r.actions, vec![Action::WarnStaleSession]);
    }

    #[test]
    fn test_active_recorded_session_stop() {
        let r = transition(
            Phase::ActiveRecorded,
            Event::SessionStop,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Ended);
        assert_eq!(r.actions, vec![Action::UpdateInteraction]);
    }

    // Transition: from Ended

    #[test]
    fn test_ended_turn_start_reenter() {
        let r = transition(Phase::Ended, Event::TurnStart, TransitionContext::default());
        assert_eq!(r.new_phase, Phase::Active);
        assert_eq!(
            r.actions,
            vec![Action::ClearEndedAt, Action::UpdateInteraction]
        );
    }

    #[test]
    fn test_ended_turn_end_noop() {
        let r = transition(Phase::Ended, Event::TurnEnd, TransitionContext::default());
        assert_eq!(r.new_phase, Phase::Ended);
        assert!(r.is_noop());
    }

    #[test]
    fn test_ended_recorded_with_files() {
        let ctx = TransitionContext::with_files_changed();
        let r = transition(Phase::Ended, Event::Recorded, ctx);
        assert_eq!(r.new_phase, Phase::Ended);
        assert_eq!(
            r.actions,
            vec![Action::RecordIfChanged, Action::UpdateInteraction]
        );
    }

    #[test]
    fn test_ended_recorded_without_files() {
        let ctx = TransitionContext {
            has_files_changed: false,
        };
        let r = transition(Phase::Ended, Event::Recorded, ctx);
        assert_eq!(r.new_phase, Phase::Ended);
        assert_eq!(
            r.actions,
            vec![Action::DiscardIfNoFiles, Action::UpdateInteraction]
        );
    }

    #[test]
    fn test_ended_session_start_reenter() {
        let r = transition(
            Phase::Ended,
            Event::SessionStart,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Idle);
        assert_eq!(r.actions, vec![Action::ClearEndedAt]);
    }

    #[test]
    fn test_ended_session_stop_noop() {
        let r = transition(
            Phase::Ended,
            Event::SessionStop,
            TransitionContext::default(),
        );
        assert_eq!(r.new_phase, Phase::Ended);
        assert!(r.is_noop());
    }

    // Full lifecycle integration tests

    #[test]
    fn test_full_lifecycle_single_turn() {
        let ctx = TransitionContext::default();

        // Session starts
        let r = transition(Phase::Idle, Event::SessionStart, ctx);
        assert_eq!(r.new_phase, Phase::Idle);

        // First turn
        let r = transition(Phase::Idle, Event::TurnStart, ctx);
        assert_eq!(r.new_phase, Phase::Active);

        let r = transition(Phase::Active, Event::TurnEnd, ctx);
        assert_eq!(r.new_phase, Phase::Idle);
        assert!(r.requires_recording());

        // Session ends
        let r = transition(Phase::Idle, Event::SessionStop, ctx);
        assert_eq!(r.new_phase, Phase::Ended);
    }

    #[test]
    fn test_full_lifecycle_multi_turn() {
        let ctx = TransitionContext::default();

        // Turn 1
        let r = transition(Phase::Idle, Event::TurnStart, ctx);
        assert_eq!(r.new_phase, Phase::Active);
        let r = transition(Phase::Active, Event::TurnEnd, ctx);
        assert_eq!(r.new_phase, Phase::Idle);
        assert!(r.requires_recording());

        // Turn 2
        let r = transition(Phase::Idle, Event::TurnStart, ctx);
        assert_eq!(r.new_phase, Phase::Active);
        let r = transition(Phase::Active, Event::TurnEnd, ctx);
        assert_eq!(r.new_phase, Phase::Idle);
        assert!(r.requires_recording());

        // Turn 3
        let r = transition(Phase::Idle, Event::TurnStart, ctx);
        assert_eq!(r.new_phase, Phase::Active);
        let r = transition(Phase::Active, Event::TurnEnd, ctx);
        assert_eq!(r.new_phase, Phase::Idle);
        assert!(r.requires_recording());
    }

    #[test]
    fn test_full_lifecycle_mid_turn_record() {
        let ctx = TransitionContext::default();

        // Turn starts
        let r = transition(Phase::Idle, Event::TurnStart, ctx);
        assert_eq!(r.new_phase, Phase::Active);

        // User records mid-turn
        let r = transition(Phase::Active, Event::Recorded, ctx);
        assert_eq!(r.new_phase, Phase::ActiveRecorded);
        assert!(!r.requires_recording()); // Just UpdateInteraction

        // Turn ends — should record
        let r = transition(Phase::ActiveRecorded, Event::TurnEnd, ctx);
        assert_eq!(r.new_phase, Phase::Idle);
        assert!(r.requires_recording());
    }

    #[test]
    fn test_full_lifecycle_session_reenter() {
        let ctx = TransitionContext::default();

        // Session ends
        let r = transition(Phase::Idle, Event::SessionStop, ctx);
        assert_eq!(r.new_phase, Phase::Ended);

        // Session re-enters
        let r = transition(Phase::Ended, Event::SessionStart, ctx);
        assert_eq!(r.new_phase, Phase::Idle);
        assert!(r.actions.contains(&Action::ClearEndedAt));

        // Can start turns again
        let r = transition(Phase::Idle, Event::TurnStart, ctx);
        assert_eq!(r.new_phase, Phase::Active);
    }

    #[test]
    fn test_full_lifecycle_ctrl_c_recovery() {
        let ctx = TransitionContext::default();

        // Turn starts
        let r = transition(Phase::Idle, Event::TurnStart, ctx);
        assert_eq!(r.new_phase, Phase::Active);

        // Agent crashes, user sends another turn start
        let r = transition(Phase::Active, Event::TurnStart, ctx);
        assert_eq!(r.new_phase, Phase::Active); // Continues

        // New turn ends normally
        let r = transition(Phase::Active, Event::TurnEnd, ctx);
        assert_eq!(r.new_phase, Phase::Idle);
        assert!(r.requires_recording());
    }

    // Exhaustive transition table test

    #[test]
    fn test_all_transitions_produce_valid_phase() {
        // Ensure every (phase, event) combination returns a valid phase
        // and doesn't panic.
        let contexts = [
            TransitionContext::default(),
            TransitionContext::with_files_changed(),
        ];

        for phase in &ALL_PHASES {
            for event in &ALL_EVENTS {
                for ctx in &contexts {
                    let result = transition(*phase, *event, *ctx);

                    // Result should be one of the known phases
                    assert!(
                        ALL_PHASES.contains(&result.new_phase),
                        "Unknown phase {:?} from transition({:?}, {:?})",
                        result.new_phase,
                        phase,
                        event
                    );

                    // Actions should not be empty AND noop at the same time
                    assert_eq!(result.is_noop(), result.actions.is_empty());
                }
            }
        }
    }

    #[test]
    fn test_all_transitions_count() {
        // 4 phases × 5 events = 20 transitions × 2 contexts = 40
        // Verify we exercise them all in the exhaustive test above
        let mut count = 0;
        let contexts = [
            TransitionContext::default(),
            TransitionContext::with_files_changed(),
        ];

        for _phase in &ALL_PHASES {
            for _event in &ALL_EVENTS {
                for _ctx in &contexts {
                    count += 1;
                }
            }
        }

        assert_eq!(count, 40);
    }

    // Context tests

    #[test]
    fn test_context_default() {
        let ctx = TransitionContext::default();
        assert!(!ctx.has_files_changed);
    }

    #[test]
    fn test_context_with_files_changed() {
        let ctx = TransitionContext::with_files_changed();
        assert!(ctx.has_files_changed);
    }

    #[test]
    fn test_context_only_affects_ended_recorded() {
        // The has_files_changed flag only affects Ended + Recorded.
        // All other transitions should be identical regardless of context.
        for phase in &ALL_PHASES {
            for event in &ALL_EVENTS {
                // Skip the one transition that depends on context
                if *phase == Phase::Ended && *event == Event::Recorded {
                    continue;
                }

                let r1 = transition(
                    *phase,
                    *event,
                    TransitionContext {
                        has_files_changed: false,
                    },
                );
                let r2 = transition(
                    *phase,
                    *event,
                    TransitionContext {
                        has_files_changed: true,
                    },
                );

                assert_eq!(
                    r1, r2,
                    "Context affected non-Ended+Recorded transition ({:?}, {:?})",
                    phase, event
                );
            }
        }
    }

    // apply_common_actions tests

    /// Mock session state for testing apply_common_actions
    struct MockSession {
        phase: Phase,
        interaction_touched: Cell<bool>,
        ended_at_cleared: Cell<bool>,
    }

    impl MockSession {
        fn new() -> Self {
            Self {
                phase: Phase::Idle,
                interaction_touched: Cell::new(false),
                ended_at_cleared: Cell::new(false),
            }
        }
    }

    impl SessionState for MockSession {
        fn set_phase(&mut self, phase: Phase) {
            self.phase = phase;
        }

        fn touch_interaction(&mut self) {
            self.interaction_touched.set(true);
        }

        fn clear_ended_at(&mut self) {
            self.ended_at_cleared.set(true);
        }
    }

    #[test]
    fn test_apply_common_updates_phase() {
        let mut session = MockSession::new();
        let result = TransitionResult::with_actions(Phase::Active, vec![Action::UpdateInteraction]);

        apply_common_actions(&mut session, &result);

        assert_eq!(session.phase, Phase::Active);
    }

    #[test]
    fn test_apply_common_touches_interaction() {
        let mut session = MockSession::new();
        let result = TransitionResult::with_actions(Phase::Idle, vec![Action::UpdateInteraction]);

        apply_common_actions(&mut session, &result);

        assert!(session.interaction_touched.get());
    }

    #[test]
    fn test_apply_common_clears_ended_at() {
        let mut session = MockSession::new();
        let result = TransitionResult::with_actions(
            Phase::Idle,
            vec![Action::ClearEndedAt, Action::UpdateInteraction],
        );

        apply_common_actions(&mut session, &result);

        assert!(session.ended_at_cleared.get());
        assert!(session.interaction_touched.get());
    }

    #[test]
    fn test_apply_common_returns_strategy_actions() {
        let mut session = MockSession::new();
        let result = TransitionResult::with_actions(
            Phase::Idle,
            vec![Action::RecordTurn, Action::UpdateInteraction],
        );

        let remaining = apply_common_actions(&mut session, &result);

        assert_eq!(remaining, vec![Action::RecordTurn]);
        assert!(session.interaction_touched.get());
    }

    #[test]
    fn test_apply_common_returns_all_strategy_actions() {
        let mut session = MockSession::new();
        let result = TransitionResult::with_actions(
            Phase::Ended,
            vec![
                Action::RecordIfChanged,
                Action::UpdateInteraction,
                Action::WarnStaleSession,
            ],
        );

        let remaining = apply_common_actions(&mut session, &result);

        assert_eq!(
            remaining,
            vec![Action::RecordIfChanged, Action::WarnStaleSession]
        );
    }

    #[test]
    fn test_apply_common_noop_result() {
        let mut session = MockSession::new();
        let result = TransitionResult::noop(Phase::Idle);

        let remaining = apply_common_actions(&mut session, &result);

        assert!(remaining.is_empty());
        assert!(!session.interaction_touched.get());
        assert!(!session.ended_at_cleared.get());
    }

    // Integration: transition + apply_common_actions

    #[test]
    fn test_transition_and_apply_turn_lifecycle() {
        let mut session = MockSession::new();
        let ctx = TransitionContext::default();

        // TurnStart
        let result = transition(Phase::Idle, Event::TurnStart, ctx);
        let remaining = apply_common_actions(&mut session, &result);
        assert_eq!(session.phase, Phase::Active);
        assert!(remaining.is_empty());
        assert!(session.interaction_touched.get());

        // Reset tracking
        session.interaction_touched.set(false);

        // TurnEnd
        let result = transition(Phase::Active, Event::TurnEnd, ctx);
        let remaining = apply_common_actions(&mut session, &result);
        assert_eq!(session.phase, Phase::Idle);
        assert_eq!(remaining, vec![Action::RecordTurn]);
        assert!(session.interaction_touched.get());
    }

    #[test]
    fn test_transition_and_apply_session_reenter() {
        let mut session = MockSession::new();
        session.phase = Phase::Ended;
        let ctx = TransitionContext::default();

        let result = transition(Phase::Ended, Event::SessionStart, ctx);
        let remaining = apply_common_actions(&mut session, &result);

        assert_eq!(session.phase, Phase::Idle);
        assert!(session.ended_at_cleared.get());
        assert!(remaining.is_empty());
    }
}
