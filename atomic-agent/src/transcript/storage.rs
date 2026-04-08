use serde::{Deserialize, Serialize};

use super::condense::{aggregate_tool_usage, extract_prompts, format_condensed};
use super::reasoning::TurnReasoning;
use super::types::{CondensedEntry, ToolUseSummary};

// Unhashed Turn Data

/// The top-level unhashed data for an agent turn.
///
/// Stored in `change.unhashed["agent_turn"]`. Contains the condensed
/// transcript, extracted prompts, tool usage, and optional AI-generated
/// reasoning summary.
///
/// This is unhashed so it can be:
/// - Stripped from public repos (set `redacted: true`)
/// - Regenerated with a better model
/// - Different across clones (interpretation, not fact)
///
/// The change hash is NOT affected by any of this data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnhashedTurnData {
    /// Session identifier (links to other turns in the same session).
    pub session_id: String,

    /// Turn number within the session (1-indexed).
    pub turn_number: u32,

    /// Transcript format: "jsonl" (Claude), "json" (Gemini), "markdown" (other).
    pub transcript_format: String,

    /// Structured condensed transcript entries.
    pub condensed_transcript: Vec<CondensedEntry>,

    /// Human-readable formatted transcript text.
    ///
    /// This is `format_condensed()` output — the `[User]/[Assistant]/[Tool]`
    /// format used for display and as input to the reasoning generator.
    pub condensed_text: String,

    /// Extracted user prompts for quick access and search.
    #[serde(default)]
    pub prompts: Vec<String>,

    /// Aggregated tool usage statistics.
    #[serde(default)]
    pub tools_used: Vec<ToolUseSummary>,

    /// AI-generated reasoning summary.
    ///
    /// `None` if reasoning generation is disabled or failed.
    /// Contains intent, outcome, learnings, friction, and open items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<TurnReasoning>,

    /// Whether the transcript/reasoning has been stripped for privacy.
    ///
    /// When `true`, `condensed_transcript` and `reasoning` are empty/None.
    /// The server shows a "redacted" badge instead of the transcript viewer.
    #[serde(default)]
    pub redacted: bool,
}

impl UnhashedTurnData {
    /// Create a minimal unhashed data struct (no reasoning).
    pub fn new(
        session_id: impl Into<String>,
        turn_number: u32,
        format: impl Into<String>,
        entries: Vec<CondensedEntry>,
        files: &[String],
    ) -> Self {
        let condensed_text = format_condensed(&entries, files);
        let prompts = extract_prompts(&entries);
        let tools_used = aggregate_tool_usage(&entries);

        Self {
            session_id: session_id.into(),
            turn_number,
            transcript_format: format.into(),
            condensed_transcript: entries,
            condensed_text,
            prompts,
            tools_used,
            reasoning: None,
            redacted: false,
        }
    }

    /// Set the reasoning summary.
    pub fn with_reasoning(mut self, reasoning: TurnReasoning) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Returns true if this data has a reasoning summary.
    pub fn has_reasoning(&self) -> bool {
        self.reasoning.is_some()
    }

    /// Returns true if this data has been redacted.
    pub fn is_redacted(&self) -> bool {
        self.redacted
    }

    /// Returns the number of condensed transcript entries.
    pub fn entry_count(&self) -> usize {
        self.condensed_transcript.len()
    }
}

// Attach / Extract / Strip operations on Change.unhashed

/// The JSON key used to store agent turn data in `change.unhashed`.
pub const UNHASHED_KEY: &str = "agent_turn";

/// Attach unhashed turn data to a change.
///
/// Serializes `UnhashedTurnData` as JSON and stores it under the
/// `"agent_turn"` key in `change.unhashed`. Creates the unhashed
/// JSON object if it doesn't exist.
pub fn attach_unhashed(
    change: &mut atomic_core::change::Change,
    data: &UnhashedTurnData,
) -> Result<(), serde_json::Error> {
    let value = serde_json::to_value(data)?;

    let unhashed = change.unhashed.get_or_insert_with(|| serde_json::json!({}));

    if let Some(obj) = unhashed.as_object_mut() {
        obj.insert(UNHASHED_KEY.to_string(), value);
    }

    Ok(())
}

/// Extract unhashed turn data from a change.
///
/// Returns `None` if the change has no unhashed data or no `"agent_turn"` key.
pub fn extract_unhashed(change: &atomic_core::change::Change) -> Option<UnhashedTurnData> {
    let unhashed = change.unhashed.as_ref()?;
    let agent_turn = unhashed.get(UNHASHED_KEY)?;
    serde_json::from_value(agent_turn.clone()).ok()
}

/// Strip transcript and reasoning from a change for privacy.
///
/// Replaces the unhashed turn data with a minimal stub that has
/// `redacted: true`. The change hash is NOT affected.
///
/// Returns `true` if data was stripped, `false` if there was nothing to strip.
pub fn strip_unhashed(change: &mut atomic_core::change::Change) -> bool {
    let Some(unhashed) = change.unhashed.as_mut() else {
        return false;
    };
    let Some(obj) = unhashed.as_object_mut() else {
        return false;
    };

    if !obj.contains_key(UNHASHED_KEY) {
        return false;
    }

    // Extract just the session_id and turn_number for the stub
    let stub = if let Some(existing) = obj.get(UNHASHED_KEY) {
        let session_id = existing
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let turn_number = existing
            .get("turn_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        serde_json::json!({
            "session_id": session_id,
            "turn_number": turn_number,
            "transcript_format": "redacted",
            "condensed_transcript": [],
            "condensed_text": "",
            "prompts": [],
            "tools_used": [],
            "reasoning": null,
            "redacted": true
        })
    } else {
        serde_json::json!({ "redacted": true })
    };

    obj.insert(UNHASHED_KEY.to_string(), stub);
    true
}

/// Check if a change has unhashed agent turn data.
pub fn has_unhashed(change: &atomic_core::change::Change) -> bool {
    change
        .unhashed
        .as_ref()
        .and_then(|v| v.get(UNHASHED_KEY))
        .is_some()
}

/// Check if a change's unhashed data has been redacted.
pub fn is_redacted(change: &atomic_core::change::Change) -> bool {
    change
        .unhashed
        .as_ref()
        .and_then(|v| v.get(UNHASHED_KEY))
        .and_then(|v| v.get("redacted"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}
