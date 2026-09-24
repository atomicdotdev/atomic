//! Turn state machine and session management.
//!
//! This module contains the turn lifecycle state machine (Phase 17 in the
//! task list), including:
//!
//! - **[`phase`]** — Phase/Event/Action model ported from Entire CLI's
//!   `session/phase.go`, with actions mapped to Atomic operations instead
//!   of git shadow branch operations. **✅ Implemented.**
//!
//! - **`session`** — `AgentSession` struct and `SessionStore` for persisting
//!   session state as JSON in `.atomic/sessions/`. _(Planned: Phase 17.2)_
//!
//! - **`capture`** — authenticated managed commit-time capture evidence
//!   (CB-12A, RFC §10.3): one MAC-bound capture per active session written by
//!   the pre-commit hook and verified at turn end. A verified capture is
//!   evidence only — the `ManagedGitCommitCaptured` classification still
//!   requires the RFC §10.3.2 reassembly, which is unimplemented while RFC
//!   §19 Q2 is undecided.
//!
//! - **`orchestrator`** — `TurnOrchestrator` that dispatches `TurnEvent`s
//!   through the state machine, manages the `FileWatcher`, and calls
//!   `record_turn()` to create Atomic changes. _(Planned: Phase 17.3)_
//!
//! # State Machine Overview
//!
//! ```text
//!                  TurnStart              TurnEnd
//!        ┌───────────────────┐  ┌────────────────────┐
//!        │                   │  │                     │
//!        ▼                   │  ▼                     │
//!   ┌─────────┐        ┌─────────┐             ┌─────────┐
//!   │  Idle   │───────▶│ Active  │────────────▶│  Idle   │
//!   └─────────┘        └─────────┘  RecordTurn └─────────┘
//!        │                   │
//!        │ SessionStop       │ Recorded (mid-turn)
//!        ▼                   ▼
//!   ┌─────────┐        ┌───────────────┐
//!   │  Ended  │        │ActiveRecorded │──TurnEnd──▶ Idle + RecordTurn
//!   └─────────┘        └───────────────┘
//! ```
//!
//! # Key Difference from Entire CLI
//!
//! Entire CLI's state machine has `ActionMigrateShadowBranch` and
//! `IsRebaseInProgress` paths — both are deleted here because Atomic
//! doesn't use git shadow branches or rebase. The `ActionCondense`
//! action becomes `RecordTurn` (record the turn as an Atomic change).
//!
//! # Implementation Status
//!
//! - [x] `phase.rs` — `Phase`, `Event`, `Action`, `transition()` pure function
//! - [x] `session.rs` — `AgentSession`, `SessionStore`
//! - [x] `orchestrator.rs` — `TurnOrchestrator`

pub mod capture;
pub mod orchestrator;
pub mod phase;
pub mod session;

// Re-export primary types for convenience
pub use orchestrator::{
    DispatchResult, JournalAppendAck, JournalCheckpointAttempt, JournalCheckpointSource,
    JournalStopCause, JournalTurnLifecycle, JournalTurnReservation, JournalTurnStatus,
    ProvenanceJournalSink, TurnOrchestrator,
};
pub use phase::{
    apply_common_actions, transition, Action, Event, Phase, SessionState, TransitionContext,
    TransitionResult,
};
pub use session::{AgentSession, SessionStore};
