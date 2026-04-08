//! Turn recording option and outcome types.

use atomic_core::types::{Base32, Hash};

use crate::event::TurnEvent;
use crate::transcript;
use crate::turn::session::AgentSession;

/// Options for recording an agent turn as an Atomic change.
///
/// Bundles together all the data needed to create a change from a completed
/// turn. The caller (orchestrator) collects this data from the session state
/// and the hook's `TurnEvent`.
///
/// The recording workflow is: **status → add untracked → record all**.
/// The repository compares the working copy against the pristine (last
/// recorded state) to determine what changed. Any files the agent created
/// (untracked) are automatically added before recording.
#[derive(Debug)]
pub struct TurnRecordOptions<'a> {
    /// The current session state.
    pub session: &'a AgentSession,

    /// The turn-end event that triggered recording.
    pub event: &'a TurnEvent,

    /// The turn number being recorded (1-indexed).
    ///
    /// This is typically `session.turn_count + 1` (before incrementing)
    /// or the value returned by `session.end_turn()`.
    pub turn_number: u32,

    /// Wall-clock duration of this turn in milliseconds.
    pub turn_duration_ms: u64,

    /// The user's prompt for this turn, if available.
    ///
    /// Used for the change message and the SessionEnvelope's prompt_summary.
    pub prompt: Option<String>,
}

/// The result of recording a turn as an Atomic change.
#[derive(Debug)]
pub struct TurnRecordOutcome {
    /// The hash of the recorded change.
    pub hash: Hash,

    /// The turn number that was recorded.
    pub turn_number: u32,

    /// Number of files in the change.
    pub file_count: usize,

    /// The change message that was used.
    pub message: String,

    /// List of files that were recorded in this turn.
    ///
    /// Includes modified, added, and deleted files. Used by the orchestrator
    /// to update `AgentSession.files_touched`.
    pub(crate) recorded_files: Vec<String>,

    /// Unhashed turn data (transcript + reasoning) ready to be attached.
    ///
    /// Built from the agent's transcript file after recording. Contains the
    /// condensed transcript, extracted prompts, tool usage, and optional
    /// AI-generated reasoning summary. `None` if the transcript was not
    /// available or could not be parsed.
    pub unhashed_data: Option<transcript::UnhashedTurnData>,
}

impl TurnRecordOutcome {
    /// Returns the list of files that were recorded in this turn.
    pub fn recorded_file_list(&self) -> &[String] {
        &self.recorded_files
    }

    /// Returns the unhashed turn data (transcript + reasoning), if available.
    pub fn unhashed(&self) -> Option<&transcript::UnhashedTurnData> {
        self.unhashed_data.as_ref()
    }

    /// Returns true if reasoning was generated for this turn.
    pub fn has_reasoning(&self) -> bool {
        self.unhashed_data
            .as_ref()
            .is_some_and(|d: &transcript::UnhashedTurnData| d.has_reasoning())
    }
}

impl std::fmt::Display for TurnRecordOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Turn {} recorded as {} ({} file{})",
            self.turn_number,
            self.hash.to_base32(),
            self.file_count,
            if self.file_count == 1 { "" } else { "s" },
        )
    }
}
