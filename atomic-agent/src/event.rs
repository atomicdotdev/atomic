//! Event types for agent turn lifecycle.
//!
//! This module defines the core event types that flow through the atomic-agent
//! system. Agent hooks emit [`TurnEvent`]s, the file watcher produces
//! [`TurnChanges`], and the orchestrator uses both to record Atomic changes.
//!
//! # Event Flow
//!
//! ```text
//! Agent Hook (stdin JSON)
//!       │
//!       ▼
//!   TurnEvent { hook_type, session_id, prompt, ... }
//!       │
//!       ├──▶ Orchestrator (state machine transition)
//!       │
//!       ├──▶ Watcher.begin_turn() / end_turn()
//!       │         │
//!       │         ▼
//!       │    TurnChanges { modified, added, deleted }
//!       │         │
//!       └─────────┴──▶ record_turn() → Atomic Change
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::event::{HookType, TurnEvent, TurnChanges};
//! use std::path::PathBuf;
//!
//! // Parse a hook callback into a TurnEvent
//! let event = TurnEvent::new("session-abc", HookType::TurnStart)
//!     .with_prompt("Fix the authentication bug in login.rs");
//!
//! assert_eq!(event.session_id, "session-abc");
//! assert!(event.prompt.is_some());
//!
//! // After a turn, the watcher reports what changed
//! let changes = TurnChanges::new()
//!     .with_modified(vec![PathBuf::from("src/auth.rs")])
//!     .with_added(vec![PathBuf::from("src/auth_test.rs")]);
//!
//! assert_eq!(changes.file_count(), 2);
//! assert!(!changes.is_empty());
//! ```

use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// HookType

/// The type of lifecycle hook that triggered an event.
///
/// These map to agent-specific hook names:
///
/// | HookType       | Claude Code            | Gemini CLI        | Hermes Agent     |
/// |----------------|------------------------|-------------------|------------------|
/// | SessionStart   | session-start          | session-start     | on_session_start |
/// | SessionEnd     | session-end            | session-end       | on_session_end   |
/// | TurnStart      | user-prompt-submit     | before-model      | pre_llm_call     |
/// | TurnEnd        | stop                   | after-model       | post_llm_call    |
/// | PreToolUse     | pre-task               | before-tool       | pre_tool_call    |
/// | PostToolUse    | post-task / post-todo  | after-tool        | post_tool_call   |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    /// Agent session process started.
    ///
    /// Fired once when the user opens a new conversation with the agent.
    /// Creates or resumes an `AgentSession`.
    SessionStart,

    /// Agent session process ended.
    ///
    /// Fired when the agent process exits (user closes the conversation).
    /// Transitions the session to `Phase::Ended`.
    SessionEnd,

    /// A new agent turn is starting (user submitted a prompt).
    ///
    /// This is the signal to snapshot the filesystem state (via Watchman
    /// `clock` + `state_enter`) so we can diff at turn end.
    ///
    /// Maps to Claude Code's `user-prompt-submit` and Gemini CLI's `before-model`.
    TurnStart,

    /// The agent turn has completed (agent finished responding).
    ///
    /// This is the signal to query Watchman for files changed since
    /// `TurnStart`, then record those changes as an Atomic change.
    ///
    /// Maps to Claude Code's `stop` and Gemini CLI's `after-model`.
    TurnEnd,

    /// The agent is about to invoke a tool.
    ///
    /// Used for sub-turn granularity — e.g., tracking when the agent
    /// spawns a sub-agent (Task tool in Claude Code).
    PreToolUse,

    /// The agent finished invoking a tool.
    ///
    /// Used to capture tool results, especially for sub-agent completion
    /// and incremental checkpoints (TodoWrite, Edit tools).
    PostToolUse,
}

impl HookType {
    /// Returns `true` if this hook type represents a turn boundary.
    ///
    /// Turn boundaries are the events that trigger filesystem snapshots
    /// and change recording.
    pub fn is_turn_boundary(&self) -> bool {
        matches!(self, HookType::TurnStart | HookType::TurnEnd)
    }

    /// Returns `true` if this hook type represents a session boundary.
    pub fn is_session_boundary(&self) -> bool {
        matches!(self, HookType::SessionStart | HookType::SessionEnd)
    }

    /// Returns `true` if this hook type represents a tool use event.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, HookType::PreToolUse | HookType::PostToolUse)
    }

    /// Parse a hook type from an agent-specific verb string.
    ///
    /// Each agent uses different names for the same lifecycle events.
    /// This normalizes them to a common `HookType`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_agent::event::HookType;
    ///
    /// // Claude Code verbs
    /// assert_eq!(HookType::from_verb("user-prompt-submit"), Some(HookType::TurnStart));
    /// assert_eq!(HookType::from_verb("stop"), Some(HookType::TurnEnd));
    ///
    /// // Gemini CLI verbs
    /// assert_eq!(HookType::from_verb("before-agent"), Some(HookType::TurnStart));
    /// assert_eq!(HookType::from_verb("after-agent"), Some(HookType::TurnEnd));
    ///
    /// // OpenCode verbs
    /// assert_eq!(HookType::from_verb("user-prompt"), Some(HookType::TurnStart));
    ///
    /// // Kiro IDE verbs
    /// assert_eq!(HookType::from_verb("prompt-submit"), Some(HookType::TurnStart));
    /// assert_eq!(HookType::from_verb("agent-stop"), Some(HookType::TurnEnd));
    ///
    /// // Hermes Agent verbs
    /// assert_eq!(HookType::from_verb("pre_llm_call"), Some(HookType::TurnStart));
    /// assert_eq!(HookType::from_verb("post_tool_call"), Some(HookType::PostToolUse));
    ///
    /// // Unknown verb
    /// assert_eq!(HookType::from_verb("unknown"), None);
    /// ```
    pub fn from_verb(verb: &str) -> Option<Self> {
        match verb {
            // Session boundaries (shared by Claude Code, Copilot, Gemini CLI, OpenCode, Pi, Sherpa)
            "session-start" => Some(HookType::SessionStart),
            "session-end" => Some(HookType::SessionEnd),

            // Cline task lifecycle
            "task-start" | "task-resume" => Some(HookType::SessionStart),
            "task-complete" => Some(HookType::TurnEnd),
            "task-cancel" => Some(HookType::SessionEnd),

            // Claude Code turn boundaries
            "user-prompt-submit" => Some(HookType::TurnStart),
            "stop" => Some(HookType::TurnEnd),

            // Hermes Agent plugin/shell hook events
            "on-session-start" | "on_session_start" => Some(HookType::SessionStart),
            "on-session-end" | "on_session_end" => Some(HookType::SessionEnd),
            "pre-llm-call" | "pre_llm_call" => Some(HookType::TurnStart),
            "post-llm-call" | "post_llm_call" => Some(HookType::TurnEnd),

            // Gemini CLI turn boundaries
            "before-agent" => Some(HookType::TurnStart),
            "after-agent" => Some(HookType::TurnEnd),

            // Copilot + OpenCode + Pi turn boundaries
            "user-prompt" => Some(HookType::TurnStart),
            // OpenCode + Pi also use "stop" for TurnEnd (handled above)
            // Copilot has no stop/TurnEnd event — records on sessionEnd

            // Sherpa TUI turn boundaries
            "turn-start" => Some(HookType::TurnStart),
            "turn-end" => Some(HookType::TurnEnd),
            // Sherpa verification triggers turn-end
            "verification" => Some(HookType::TurnEnd),

            // Claude Code tool use
            "pre-task" => Some(HookType::PreToolUse),
            "post-task" | "post-todo" | "post-tool" => Some(HookType::PostToolUse),

            // Codex + Copilot + Hermes pre-tool use
            "pre-tool" | "pre_tool_call" => Some(HookType::PreToolUse),

            // Cursor thinking blocks
            "after-thought" => Some(HookType::PostToolUse),

            // Gemini CLI / OpenCode / Pi tool use (shared verbs)
            "before-tool" => Some(HookType::PreToolUse),
            "after-tool" | "post_tool_call" => Some(HookType::PostToolUse),

            // Kiro IDE turn boundaries
            "prompt-submit" => Some(HookType::TurnStart),
            "agent-stop" => Some(HookType::TurnEnd),
            // Kiro tool use
            "pre-tool-use" => Some(HookType::PreToolUse),
            "post-tool-use" => Some(HookType::PostToolUse),
            // Kiro post-task is already handled above ("post-task" → PostToolUse)
            _ => None,
        }
    }

    /// Returns the canonical string representation of this hook type.
    pub fn as_str(&self) -> &'static str {
        match self {
            HookType::SessionStart => "session_start",
            HookType::SessionEnd => "session_end",
            HookType::TurnStart => "turn_start",
            HookType::TurnEnd => "turn_end",
            HookType::PreToolUse => "pre_tool_use",
            HookType::PostToolUse => "post_tool_use",
        }
    }
}

impl fmt::Display for HookType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// TurnEvent

/// A normalized event from an agent hook callback.
///
/// Agent hooks (Claude Code, Gemini CLI, Codex, OpenCode) each send different
/// JSON formats on stdin. The agent-specific [`AgentHook`](crate::hooks::AgentHook)
/// implementations parse that JSON into this common type.
///
/// # Required vs Optional Fields
///
/// - `session_id` and `event_type` are always present.
/// - `prompt` is only present on `TurnStart` events.
/// - `tool_name` and `tool_use_id` are only present on tool use events.
/// - `transcript_path` depends on the agent (Claude Code always provides it).
/// - `raw_json` preserves the original input for debugging.
///
/// # Example
///
/// ```rust
/// use atomic_agent::event::{HookType, TurnEvent};
///
/// let event = TurnEvent::new("sess-123", HookType::TurnEnd);
/// assert_eq!(event.session_id, "sess-123");
/// assert_eq!(event.event_type, HookType::TurnEnd);
/// assert!(event.prompt.is_none());
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnEvent {
    /// The agent-assigned session identifier.
    ///
    /// For Claude Code this is a UUID. For Gemini CLI it may be path-derived.
    /// The `AgentHook` implementation may transform this into an Atomic session
    /// ID (e.g., prepending a date prefix).
    pub session_id: String,

    /// What kind of lifecycle event this represents.
    pub event_type: HookType,

    /// Path to the agent's transcript file, if available.
    ///
    /// Claude Code provides this as a JSONL file path. Gemini CLI provides
    /// it as a JSON file path. Other agents may not provide it at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,

    /// The user's prompt text, if this is a `TurnStart` event.
    ///
    /// Only populated for `HookType::TurnStart`. May be truncated by the
    /// agent hook adapter for very long prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,

    /// The name of the tool being invoked, for tool use events.
    ///
    /// Only populated for `HookType::PreToolUse` and `HookType::PostToolUse`.
    /// Examples: "Task", "Edit", "Write", "TodoWrite".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,

    /// The unique identifier for a tool invocation.
    ///
    /// Only populated for tool use events. Used to correlate pre/post pairs
    /// and to identify sub-agent (Task) invocations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,

    /// When this event occurred.
    pub timestamp: DateTime<Utc>,

    /// The raw JSON input from the agent hook, preserved for debugging.
    ///
    /// This is the unparsed stdin content. Useful for diagnosing hook
    /// parsing issues or extracting agent-specific fields not captured
    /// in the common `TurnEvent` type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_json: Option<serde_json::Value>,
}

impl TurnEvent {
    /// Create a new `TurnEvent` with the required fields.
    ///
    /// The timestamp is set to `Utc::now()`. Use [`with_timestamp`](Self::with_timestamp)
    /// to override.
    pub fn new(session_id: impl Into<String>, event_type: HookType) -> Self {
        Self {
            session_id: session_id.into(),
            event_type,
            transcript_path: None,
            prompt: None,
            tool_name: None,
            tool_use_id: None,
            timestamp: Utc::now(),
            raw_json: None,
        }
    }

    /// Set the transcript path.
    #[must_use]
    pub fn with_transcript_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.transcript_path = Some(path.into());
        self
    }

    /// Set the user prompt.
    #[must_use]
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }

    /// Set the tool name.
    #[must_use]
    pub fn with_tool_name(mut self, name: impl Into<String>) -> Self {
        self.tool_name = Some(name.into());
        self
    }

    /// Set the tool use ID.
    #[must_use]
    pub fn with_tool_use_id(mut self, id: impl Into<String>) -> Self {
        self.tool_use_id = Some(id.into());
        self
    }

    /// Set the timestamp.
    #[must_use]
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = timestamp;
        self
    }

    /// Attach the raw JSON input for debugging.
    #[must_use]
    pub fn with_raw_json(mut self, raw: serde_json::Value) -> Self {
        self.raw_json = Some(raw);
        self
    }

    /// Returns `true` if this event has a user prompt attached.
    pub fn has_prompt(&self) -> bool {
        self.prompt.as_ref().is_some_and(|p| !p.is_empty())
    }

    /// Returns the prompt truncated to `max_len` characters.
    ///
    /// Useful for display purposes (commit messages, UI previews).
    /// Returns `None` if no prompt is present.
    pub fn prompt_summary(&self, max_len: usize) -> Option<String> {
        self.prompt.as_ref().map(|p| {
            if p.len() <= max_len {
                p.clone()
            } else {
                let truncated: String = p.chars().take(max_len.saturating_sub(3)).collect();
                format!("{}...", truncated)
            }
        })
    }

    /// Returns `true` if this event represents a sub-agent (Task) tool use.
    ///
    /// Sub-agent tool uses are special because they may warrant their own
    /// sub-turn recording.
    pub fn is_sub_agent(&self) -> bool {
        self.tool_name.as_deref() == Some("Task")
    }
}

impl fmt::Display for TurnEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {} session={}",
            self.timestamp.format("%H:%M:%S"),
            self.event_type,
            self.session_id,
        )?;
        if let Some(prompt) = self.prompt_summary(60) {
            write!(f, " prompt=\"{}\"", prompt)?;
        }
        if let Some(tool) = &self.tool_name {
            write!(f, " tool={}", tool)?;
        }
        Ok(())
    }
}

// TurnChanges

/// Files that changed during a single agent turn.
///
/// Produced by the [`FileWatcher`](crate::watcher::FileWatcher) at turn end.
/// The Watchman-based watcher uses a `since`-clock query to get exact changes;
/// the fallback watcher diffs two filesystem snapshots.
///
/// These file lists are passed to `atomic-repository`'s `record()` to create
/// the Atomic change for this turn.
///
/// # Example
///
/// ```rust
/// use atomic_agent::event::TurnChanges;
/// use std::path::PathBuf;
///
/// let changes = TurnChanges::new()
///     .with_modified(vec![PathBuf::from("src/main.rs")])
///     .with_added(vec![PathBuf::from("src/new_file.rs")])
///     .with_deleted(vec![PathBuf::from("src/old_file.rs")]);
///
/// assert_eq!(changes.file_count(), 3);
/// assert!(!changes.is_empty());
/// assert_eq!(changes.modified.len(), 1);
/// assert_eq!(changes.added.len(), 1);
/// assert_eq!(changes.deleted.len(), 1);
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TurnChanges {
    /// Files that existed before the turn and were modified during it.
    pub modified: Vec<PathBuf>,

    /// Files that were created during the turn (did not exist before).
    pub added: Vec<PathBuf>,

    /// Files that were deleted during the turn (existed before, gone now).
    pub deleted: Vec<PathBuf>,

    /// When the changes were captured (typically the turn end time).
    pub timestamp: DateTime<Utc>,
}

impl TurnChanges {
    /// Create an empty `TurnChanges` with timestamp set to now.
    pub fn new() -> Self {
        Self {
            modified: Vec::new(),
            added: Vec::new(),
            deleted: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    /// Set the modified files list.
    #[must_use]
    pub fn with_modified(mut self, files: Vec<PathBuf>) -> Self {
        self.modified = files;
        self
    }

    /// Set the added files list.
    #[must_use]
    pub fn with_added(mut self, files: Vec<PathBuf>) -> Self {
        self.added = files;
        self
    }

    /// Set the deleted files list.
    #[must_use]
    pub fn with_deleted(mut self, files: Vec<PathBuf>) -> Self {
        self.deleted = files;
        self
    }

    /// Set the timestamp.
    #[must_use]
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = timestamp;
        self
    }

    /// Returns `true` if no files changed during this turn.
    ///
    /// An empty turn produces no Atomic change — the orchestrator
    /// silently skips recording.
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.added.is_empty() && self.deleted.is_empty()
    }

    /// Returns the total number of files affected (modified + added + deleted).
    pub fn file_count(&self) -> usize {
        self.modified.len() + self.added.len() + self.deleted.len()
    }

    /// Returns all affected file paths in a single iterator.
    ///
    /// Order: modified files first, then added, then deleted.
    /// This is the list passed to `RecordOptions::paths` when recording
    /// the turn as an Atomic change.
    pub fn all_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(self.file_count());
        paths.extend(self.modified.iter().cloned());
        paths.extend(self.added.iter().cloned());
        paths.extend(self.deleted.iter().cloned());
        paths
    }

    /// Returns all affected paths as string slices.
    ///
    /// Paths that are not valid UTF-8 are skipped. This is the format
    /// needed by `RecordOptions::paths` which takes `Vec<String>`.
    pub fn all_path_strings(&self) -> Vec<String> {
        self.all_paths()
            .iter()
            .filter_map(|p| p.to_str().map(|s| s.to_string()))
            .collect()
    }

    /// Merge another `TurnChanges` into this one.
    ///
    /// Used when combining sub-turn changes (e.g., from multiple tool
    /// invocations within a single turn). Deduplicates paths.
    pub fn merge(&mut self, other: &TurnChanges) {
        merge_paths(&mut self.modified, &other.modified);
        merge_paths(&mut self.added, &other.added);
        merge_paths(&mut self.deleted, &other.deleted);

        // Use the later timestamp
        if other.timestamp > self.timestamp {
            self.timestamp = other.timestamp;
        }
    }

    /// Returns a summary string suitable for log messages.
    ///
    /// # Example output
    ///
    /// ```text
    /// "3 files changed (1 modified, 1 added, 1 deleted)"
    /// ```
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "no files changed".to_string();
        }

        let mut parts = Vec::new();
        if !self.modified.is_empty() {
            parts.push(format!("{} modified", self.modified.len()));
        }
        if !self.added.is_empty() {
            parts.push(format!("{} added", self.added.len()));
        }
        if !self.deleted.is_empty() {
            parts.push(format!("{} deleted", self.deleted.len()));
        }

        format!(
            "{} file{} changed ({})",
            self.file_count(),
            if self.file_count() == 1 { "" } else { "s" },
            parts.join(", "),
        )
    }
}

impl fmt::Display for TurnChanges {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

// Helpers

/// Merge `source` paths into `target`, deduplicating.
fn merge_paths(target: &mut Vec<PathBuf>, source: &[PathBuf]) {
    for path in source {
        if !target.contains(path) {
            target.push(path.clone());
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // HookType tests

    #[test]
    fn test_hook_type_is_turn_boundary() {
        assert!(HookType::TurnStart.is_turn_boundary());
        assert!(HookType::TurnEnd.is_turn_boundary());
        assert!(!HookType::SessionStart.is_turn_boundary());
        assert!(!HookType::SessionEnd.is_turn_boundary());
        assert!(!HookType::PreToolUse.is_turn_boundary());
        assert!(!HookType::PostToolUse.is_turn_boundary());
    }

    #[test]
    fn test_hook_type_is_session_boundary() {
        assert!(HookType::SessionStart.is_session_boundary());
        assert!(HookType::SessionEnd.is_session_boundary());
        assert!(!HookType::TurnStart.is_session_boundary());
        assert!(!HookType::TurnEnd.is_session_boundary());
    }

    #[test]
    fn test_hook_type_is_tool_use() {
        assert!(HookType::PreToolUse.is_tool_use());
        assert!(HookType::PostToolUse.is_tool_use());
        assert!(!HookType::TurnStart.is_tool_use());
        assert!(!HookType::SessionStart.is_tool_use());
    }

    #[test]
    fn test_hook_type_from_verb_claude_code() {
        assert_eq!(
            HookType::from_verb("session-start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(
            HookType::from_verb("session-end"),
            Some(HookType::SessionEnd)
        );
        assert_eq!(
            HookType::from_verb("user-prompt-submit"),
            Some(HookType::TurnStart)
        );
        assert_eq!(HookType::from_verb("stop"), Some(HookType::TurnEnd));
        assert_eq!(HookType::from_verb("pre-task"), Some(HookType::PreToolUse));
        assert_eq!(
            HookType::from_verb("post-task"),
            Some(HookType::PostToolUse)
        );
        assert_eq!(
            HookType::from_verb("post-todo"),
            Some(HookType::PostToolUse)
        );
        assert_eq!(
            HookType::from_verb("post-tool"),
            Some(HookType::PostToolUse)
        );
    }

    #[test]
    fn test_hook_type_from_verb_gemini_cli() {
        assert_eq!(
            HookType::from_verb("before-agent"),
            Some(HookType::TurnStart)
        );
        assert_eq!(HookType::from_verb("after-agent"), Some(HookType::TurnEnd));
        assert_eq!(
            HookType::from_verb("before-tool"),
            Some(HookType::PreToolUse)
        );
        assert_eq!(
            HookType::from_verb("after-tool"),
            Some(HookType::PostToolUse)
        );
    }

    #[test]
    fn test_hook_type_from_verb_hermes() {
        assert_eq!(
            HookType::from_verb("on_session_start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(
            HookType::from_verb("on_session_end"),
            Some(HookType::SessionEnd)
        );
        assert_eq!(
            HookType::from_verb("pre_llm_call"),
            Some(HookType::TurnStart)
        );
        assert_eq!(
            HookType::from_verb("post_llm_call"),
            Some(HookType::TurnEnd)
        );
        assert_eq!(
            HookType::from_verb("pre_tool_call"),
            Some(HookType::PreToolUse)
        );
        assert_eq!(
            HookType::from_verb("post_tool_call"),
            Some(HookType::PostToolUse)
        );
    }

    #[test]
    fn test_hook_type_from_verb_unknown() {
        assert_eq!(HookType::from_verb("unknown"), None);
        assert_eq!(HookType::from_verb(""), None);
        assert_eq!(HookType::from_verb("Start"), None); // case-sensitive
    }

    #[test]
    fn test_hook_type_as_str_roundtrip() {
        let types = [
            HookType::SessionStart,
            HookType::SessionEnd,
            HookType::TurnStart,
            HookType::TurnEnd,
            HookType::PreToolUse,
            HookType::PostToolUse,
        ];
        for ht in &types {
            let s = ht.as_str();
            assert!(
                !s.is_empty(),
                "as_str() should not return empty for {:?}",
                ht
            );
        }
    }

    #[test]
    fn test_hook_type_display() {
        assert_eq!(HookType::TurnStart.to_string(), "turn_start");
        assert_eq!(HookType::PostToolUse.to_string(), "post_tool_use");
    }

    #[test]
    fn test_hook_type_serde_roundtrip() {
        let hook = HookType::TurnStart;
        let json = serde_json::to_string(&hook).unwrap();
        assert_eq!(json, "\"turn_start\"");
        let parsed: HookType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, hook);
    }

    #[test]
    fn test_hook_type_serde_all_variants() {
        let variants = [
            (HookType::SessionStart, "\"session_start\""),
            (HookType::SessionEnd, "\"session_end\""),
            (HookType::TurnStart, "\"turn_start\""),
            (HookType::TurnEnd, "\"turn_end\""),
            (HookType::PreToolUse, "\"pre_tool_use\""),
            (HookType::PostToolUse, "\"post_tool_use\""),
        ];
        for (variant, expected_json) in &variants {
            let json = serde_json::to_string(variant).unwrap();
            assert_eq!(
                &json, expected_json,
                "Serialization mismatch for {:?}",
                variant
            );
            let parsed: HookType = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, variant, "Deserialization mismatch for {}", json);
        }
    }

    // TurnEvent tests

    #[test]
    fn test_turn_event_new() {
        let event = TurnEvent::new("sess-1", HookType::TurnStart);
        assert_eq!(event.session_id, "sess-1");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert!(event.prompt.is_none());
        assert!(event.transcript_path.is_none());
        assert!(event.tool_name.is_none());
        assert!(event.tool_use_id.is_none());
        assert!(event.raw_json.is_none());
    }

    #[test]
    fn test_turn_event_builder_chain() {
        let event = TurnEvent::new("sess-1", HookType::TurnStart)
            .with_prompt("Fix the bug")
            .with_transcript_path("/tmp/transcript.jsonl")
            .with_tool_name("Edit")
            .with_tool_use_id("tu-123")
            .with_raw_json(serde_json::json!({"session_id": "sess-1"}));

        assert_eq!(event.prompt.as_deref(), Some("Fix the bug"));
        assert_eq!(
            event.transcript_path.as_deref(),
            Some(std::path::Path::new("/tmp/transcript.jsonl"))
        );
        assert_eq!(event.tool_name.as_deref(), Some("Edit"));
        assert_eq!(event.tool_use_id.as_deref(), Some("tu-123"));
        assert!(event.raw_json.is_some());
    }

    #[test]
    fn test_turn_event_has_prompt() {
        let no_prompt = TurnEvent::new("s", HookType::TurnStart);
        assert!(!no_prompt.has_prompt());

        let empty_prompt = TurnEvent::new("s", HookType::TurnStart).with_prompt("");
        assert!(!empty_prompt.has_prompt());

        let with_prompt = TurnEvent::new("s", HookType::TurnStart).with_prompt("hello");
        assert!(with_prompt.has_prompt());
    }

    #[test]
    fn test_turn_event_prompt_summary_short() {
        let event = TurnEvent::new("s", HookType::TurnStart).with_prompt("Fix the bug");
        assert_eq!(event.prompt_summary(100), Some("Fix the bug".to_string()));
    }

    #[test]
    fn test_turn_event_prompt_summary_truncated() {
        let long_prompt = "a".repeat(200);
        let event = TurnEvent::new("s", HookType::TurnStart).with_prompt(long_prompt);
        let summary = event.prompt_summary(72).unwrap();
        assert!(summary.ends_with("..."));
        assert!(summary.len() <= 72);
    }

    #[test]
    fn test_turn_event_prompt_summary_none() {
        let event = TurnEvent::new("s", HookType::TurnEnd);
        assert!(event.prompt_summary(100).is_none());
    }

    #[test]
    fn test_turn_event_prompt_summary_exact_length() {
        let prompt = "a".repeat(72);
        let event = TurnEvent::new("s", HookType::TurnStart).with_prompt(prompt.clone());
        // Exactly at limit — should NOT truncate
        assert_eq!(event.prompt_summary(72), Some(prompt));
    }

    #[test]
    fn test_turn_event_prompt_summary_unicode() {
        let prompt = "修复认证模块中的缺陷，这个问题影响了所有用户的登录体验";
        let event = TurnEvent::new("s", HookType::TurnStart).with_prompt(prompt);
        let summary = event.prompt_summary(20).unwrap();
        assert!(summary.ends_with("..."));
        // Char count should be within limit
        assert!(summary.chars().count() <= 20);
    }

    #[test]
    fn test_turn_event_is_sub_agent() {
        let task = TurnEvent::new("s", HookType::PreToolUse).with_tool_name("Task");
        assert!(task.is_sub_agent());

        let edit = TurnEvent::new("s", HookType::PreToolUse).with_tool_name("Edit");
        assert!(!edit.is_sub_agent());

        let no_tool = TurnEvent::new("s", HookType::TurnStart);
        assert!(!no_tool.is_sub_agent());
    }

    #[test]
    fn test_turn_event_with_timestamp() {
        use chrono::TimeZone;
        let ts = Utc.with_ymd_and_hms(2026, 1, 15, 10, 30, 0).unwrap();
        let event = TurnEvent::new("s", HookType::TurnStart).with_timestamp(ts);
        assert_eq!(event.timestamp, ts);
    }

    #[test]
    fn test_turn_event_display() {
        let event =
            TurnEvent::new("sess-abc", HookType::TurnStart).with_prompt("Fix authentication bug");
        let display = event.to_string();
        assert!(display.contains("turn_start"));
        assert!(display.contains("sess-abc"));
        assert!(display.contains("Fix authentication bug"));
    }

    #[test]
    fn test_turn_event_display_with_tool() {
        let event = TurnEvent::new("sess-abc", HookType::PreToolUse).with_tool_name("Task");
        let display = event.to_string();
        assert!(display.contains("tool=Task"));
    }

    #[test]
    fn test_turn_event_serde_roundtrip() {
        let event = TurnEvent::new("sess-1", HookType::TurnStart)
            .with_prompt("hello")
            .with_transcript_path("/tmp/t.jsonl");

        let json = serde_json::to_string(&event).unwrap();
        let parsed: TurnEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.session_id, event.session_id);
        assert_eq!(parsed.event_type, event.event_type);
        assert_eq!(parsed.prompt, event.prompt);
        assert_eq!(parsed.transcript_path, event.transcript_path);
    }

    #[test]
    fn test_turn_event_serde_skips_none_fields() {
        let event = TurnEvent::new("s", HookType::TurnEnd);
        let json = serde_json::to_string(&event).unwrap();
        // None fields should not appear in JSON output
        assert!(!json.contains("prompt"));
        assert!(!json.contains("tool_name"));
        assert!(!json.contains("tool_use_id"));
        assert!(!json.contains("transcript_path"));
        assert!(!json.contains("raw_json"));
    }

    // TurnChanges tests

    #[test]
    fn test_turn_changes_new_is_empty() {
        let changes = TurnChanges::new();
        assert!(changes.is_empty());
        assert_eq!(changes.file_count(), 0);
        assert!(changes.all_paths().is_empty());
    }

    #[test]
    fn test_turn_changes_with_modified() {
        let changes =
            TurnChanges::new().with_modified(vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
        assert!(!changes.is_empty());
        assert_eq!(changes.file_count(), 2);
        assert_eq!(changes.modified.len(), 2);
        assert!(changes.added.is_empty());
        assert!(changes.deleted.is_empty());
    }

    #[test]
    fn test_turn_changes_with_added() {
        let changes = TurnChanges::new().with_added(vec![PathBuf::from("new.rs")]);
        assert_eq!(changes.file_count(), 1);
        assert_eq!(changes.added.len(), 1);
    }

    #[test]
    fn test_turn_changes_with_deleted() {
        let changes = TurnChanges::new().with_deleted(vec![PathBuf::from("old.rs")]);
        assert_eq!(changes.file_count(), 1);
        assert_eq!(changes.deleted.len(), 1);
    }

    #[test]
    fn test_turn_changes_all_paths_order() {
        let changes = TurnChanges::new()
            .with_modified(vec![PathBuf::from("mod.rs")])
            .with_added(vec![PathBuf::from("add.rs")])
            .with_deleted(vec![PathBuf::from("del.rs")]);

        let paths = changes.all_paths();
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0], PathBuf::from("mod.rs"));
        assert_eq!(paths[1], PathBuf::from("add.rs"));
        assert_eq!(paths[2], PathBuf::from("del.rs"));
    }

    #[test]
    fn test_turn_changes_all_path_strings() {
        let changes = TurnChanges::new()
            .with_modified(vec![PathBuf::from("src/main.rs")])
            .with_added(vec![PathBuf::from("src/lib.rs")]);

        let strings = changes.all_path_strings();
        assert_eq!(strings, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn test_turn_changes_file_count_mixed() {
        let changes = TurnChanges::new()
            .with_modified(vec![PathBuf::from("a"), PathBuf::from("b")])
            .with_added(vec![PathBuf::from("c")])
            .with_deleted(vec![
                PathBuf::from("d"),
                PathBuf::from("e"),
                PathBuf::from("f"),
            ]);
        assert_eq!(changes.file_count(), 6);
    }

    #[test]
    fn test_turn_changes_merge_empty() {
        let mut a = TurnChanges::new().with_modified(vec![PathBuf::from("a.rs")]);
        let b = TurnChanges::new();
        a.merge(&b);
        assert_eq!(a.file_count(), 1);
    }

    #[test]
    fn test_turn_changes_merge_no_duplicates() {
        let mut a =
            TurnChanges::new().with_modified(vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
        let b =
            TurnChanges::new().with_modified(vec![PathBuf::from("b.rs"), PathBuf::from("c.rs")]);
        a.merge(&b);
        // b.rs should not be duplicated
        assert_eq!(a.modified.len(), 3);
        assert!(a.modified.contains(&PathBuf::from("a.rs")));
        assert!(a.modified.contains(&PathBuf::from("b.rs")));
        assert!(a.modified.contains(&PathBuf::from("c.rs")));
    }

    #[test]
    fn test_turn_changes_merge_takes_later_timestamp() {
        use chrono::TimeZone;
        let early = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();

        let mut a = TurnChanges::new().with_timestamp(early);
        let b = TurnChanges::new().with_timestamp(late);
        a.merge(&b);
        assert_eq!(a.timestamp, late);

        // Merge in reverse: earlier timestamp should not overwrite later
        let mut c = TurnChanges::new().with_timestamp(late);
        let d = TurnChanges::new().with_timestamp(early);
        c.merge(&d);
        assert_eq!(c.timestamp, late);
    }

    #[test]
    fn test_turn_changes_merge_across_categories() {
        let mut a = TurnChanges::new()
            .with_modified(vec![PathBuf::from("m1")])
            .with_added(vec![PathBuf::from("a1")]);
        let b = TurnChanges::new()
            .with_added(vec![PathBuf::from("a2")])
            .with_deleted(vec![PathBuf::from("d1")]);
        a.merge(&b);

        assert_eq!(a.modified.len(), 1);
        assert_eq!(a.added.len(), 2);
        assert_eq!(a.deleted.len(), 1);
        assert_eq!(a.file_count(), 4);
    }

    #[test]
    fn test_turn_changes_summary_empty() {
        let changes = TurnChanges::new();
        assert_eq!(changes.summary(), "no files changed");
    }

    #[test]
    fn test_turn_changes_summary_single_modified() {
        let changes = TurnChanges::new().with_modified(vec![PathBuf::from("a.rs")]);
        assert_eq!(changes.summary(), "1 file changed (1 modified)");
    }

    #[test]
    fn test_turn_changes_summary_multiple() {
        let changes = TurnChanges::new()
            .with_modified(vec![PathBuf::from("a"), PathBuf::from("b")])
            .with_added(vec![PathBuf::from("c")])
            .with_deleted(vec![PathBuf::from("d")]);
        assert_eq!(
            changes.summary(),
            "4 files changed (2 modified, 1 added, 1 deleted)"
        );
    }

    #[test]
    fn test_turn_changes_summary_only_added() {
        let changes = TurnChanges::new().with_added(vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(changes.summary(), "2 files changed (2 added)");
    }

    #[test]
    fn test_turn_changes_display_delegates_to_summary() {
        let changes = TurnChanges::new().with_modified(vec![PathBuf::from("x")]);
        assert_eq!(changes.to_string(), changes.summary());
    }

    #[test]
    fn test_turn_changes_serde_roundtrip() {
        let changes = TurnChanges::new()
            .with_modified(vec![PathBuf::from("src/main.rs")])
            .with_added(vec![PathBuf::from("src/new.rs")])
            .with_deleted(vec![PathBuf::from("src/old.rs")]);

        let json = serde_json::to_string(&changes).unwrap();
        let parsed: TurnChanges = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.modified, changes.modified);
        assert_eq!(parsed.added, changes.added);
        assert_eq!(parsed.deleted, changes.deleted);
        assert_eq!(parsed.file_count(), 3);
    }

    #[test]
    fn test_turn_changes_default_is_empty() {
        let changes = TurnChanges::default();
        assert!(changes.is_empty());
    }

    #[test]
    fn test_turn_changes_with_timestamp() {
        use chrono::TimeZone;
        let ts = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let changes = TurnChanges::new().with_timestamp(ts);
        assert_eq!(changes.timestamp, ts);
    }

    // merge_paths tests

    #[test]
    fn test_merge_paths_empty_source() {
        let mut target = vec![PathBuf::from("a")];
        merge_paths(&mut target, &[]);
        assert_eq!(target.len(), 1);
    }

    #[test]
    fn test_merge_paths_empty_target() {
        let mut target = Vec::new();
        merge_paths(&mut target, &[PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(target.len(), 2);
    }

    #[test]
    fn test_merge_paths_dedup() {
        let mut target = vec![PathBuf::from("a"), PathBuf::from("b")];
        merge_paths(&mut target, &[PathBuf::from("b"), PathBuf::from("c")]);
        assert_eq!(
            target,
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c"),]
        );
    }

    #[test]
    fn test_merge_paths_all_duplicates() {
        let mut target = vec![PathBuf::from("a"), PathBuf::from("b")];
        merge_paths(&mut target, &[PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(target.len(), 2);
    }
}
