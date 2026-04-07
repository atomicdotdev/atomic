//! Turn-level change recording.
//!
//! This module bridges the agent turn lifecycle into Atomic's change recording
//! system. When a turn ends, `record_turn()` takes the session state and the
//! turn event, and calls `atomic-repository`'s `record()` to create a proper
//! Atomic change with:
//!
//! - **`ChangeHeader`** — message, author, timestamp
//! - **`Provenance`** — AI vendor, model, tokens, cost, prompt hash
//! - **`SessionEnvelope`** — turn number, session context, files, timing
//!   (encoded into `HashedChange.metadata`)
//!
//! # Data Flow
//!
//! ```text
//! TurnEvent + AgentSession
//!     │
//!     ▼
//! record_turn()
//!     │
//!     ├──▶ Build ChangeHeader (message, author, timestamp)
//!     ├──▶ Build Provenance (vendor, model, tool, prompt hash)
//!     ├──▶ Build SessionEnvelope → encode → hashed.metadata
//!     ├──▶ Build RecordOptions (all: true, view, provenance)
//!     │
//!     ▼
//! repo.record(header, options)  ← repo diffs working copy vs pristine
//!     │
//!     ▼
//! RecordOutcome { change, hash, stats }
//! ```
//!
//! # Why `all: true` instead of explicit file paths?
//!
//! Each agent hook invocation is a **separate process**. The `TurnStart` hook
//! runs in one process, the agent modifies files, then the `TurnEnd` hook runs
//! in a different process. An in-memory file watcher snapshot taken in process A
//! is gone by the time process B runs.
//!
//! Instead of trying to persist watcher state across processes, we let the
//! repository do what it already does: compare the working copy against the
//! pristine (last recorded state) and record everything that changed. This is
//! the same approach Entire CLI uses with its `manual-commit` strategy.
//!
//! The Watchman integration (Phase 16.2–16.3) will improve this by querying
//! the Watchman daemon's server-side clock state, but `all: true` works now
//! and gives the right results for the demo flow.
//!
//! # Session Data in the Change
//!
//! The recorded change carries session data in three slots:
//!
//! | Slot | Data | Hashed? |
//! |---|---|---|
//! | `hashed.provenance` | tokens, cost, model, prompt hash | Yes |
//! | `hashed.metadata` | SessionEnvelope (turn#, session ID, timing) | Yes |
//! | `unhashed` | Transcript (future — Phase 18.5.3) | No |
//!
//! Because provenance and metadata are hashed, they become part of the change's
//! cryptographic identity and commute via patch theory like any other change data.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_agent::record::{record_turn, TurnRecordOptions};
//! use atomic_repository::Repository;
//!
//! let repo = Repository::open(".")?;
//! let outcome = record_turn(&repo, &TurnRecordOptions {
//!     session: &session,
//!     changes: &turn_changes,
//!     event: &turn_event,
//! })?;
//!
//! println!("Recorded turn {} as change {}", outcome.turn_number, outcome.hash);
//! ```

use std::path::Path;

use atomic_core::change::{
    AITool, AIVendor, ChangeHeader, Cost, PromptContent, Provenance, SuggestionType, TokenUsage,
};
use atomic_core::types::{Base32, Hash};

use atomic_repository::status::{FileStatus, RepositoryStatus};

use crate::envelope::SessionEnvelope;
use crate::error::{AgentError, AgentResult};
use crate::event::TurnEvent;
use crate::identity::build_agent_author;
use crate::transcript;
use crate::turn::session::AgentSession;

// TurnRecordOptions

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

// TurnRecordOutcome

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
    recorded_files: Vec<String>,

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
            .is_some_and(|d| d.has_reasoning())
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

// Change Header Construction

/// Build a `ChangeHeader` for an agent turn.
///
/// The message is built from the file changes and prompt context:
/// - Good prompt: `"Fix the authentication bug in login.rs"`
/// - Slash command or no prompt: `"Add src/main.rs, Cargo.toml"`
///
/// The author is the agent identity.
fn build_turn_header(
    options: &TurnRecordOptions<'_>,
    status: &RepositoryStatus,
    untracked_paths: &[String],
) -> ChangeHeader {
    let message = build_turn_message(options, status, untracked_paths);

    let author = build_agent_author(
        &options.session.agent_name,
        &options.session.agent_display_name,
        &options.session.session_id,
    );

    ChangeHeader::builder()
        .message(message)
        .author(author)
        .build()
}

/// Build the change message for a turn.
///
/// Message priority:
/// 1. If the prompt is meaningful (not a slash command, not too short),
///    use it directly — the user's intent is the best commit message.
/// 2. Generate a descriptive message from the actual file changes
///    (e.g., "Add CLAUDE.md" or "Modify auth.rs; add test_auth.rs").
/// 3. Extract a wrap-up summary from the agent's transcript — but ONLY
///    text that appears AFTER the last tool call. Pre-tool text is Claude
///    planning/analyzing, not summarizing what it did.
/// 4. Fall back to agent name if nothing else is available.
///
/// The message never includes a `Turn N:` prefix — the turn number is already
/// in the SessionEnvelope metadata and `atomic log` can render it from there.
/// The change message should describe *what changed*, not bookkeeping.
fn build_turn_message(
    options: &TurnRecordOptions<'_>,
    status: &RepositoryStatus,
    untracked_paths: &[String],
) -> String {
    // Priority 1: If we have a meaningful prompt, use it directly.
    // The user typed something like "Fix the auth bug" — that's the best
    // description of intent. Slash commands (/init, /help) are filtered out.
    if let Some(prompt) = &options.prompt {
        if is_meaningful_prompt(prompt) {
            return truncate_prompt(prompt, 72);
        }
    }

    // Priority 2: Generate a message from the actual file changes.
    // This is always accurate and concrete: "Add CLAUDE.md" or
    // "Modify auth.rs; add test_auth.rs, utils.rs (+3 more)".
    let file_summary = build_file_change_summary(status, untracked_paths);
    if !file_summary.is_empty() {
        return truncate_prompt(&file_summary, 72);
    }

    // Priority 3: Try the transcript as a last resort.
    // Only use assistant text that appears AFTER the last tool call —
    // that's Claude's wrap-up summary, not its opening analysis/planning.
    if let Some(ref transcript_path) = options.session.transcript_path {
        if let Some(summary) = summarize_from_transcript(transcript_path) {
            return truncate_prompt(&summary, 72);
        }
    }

    // Priority 4: Last resort
    format!(
        "Turn {} ({})",
        options.turn_number, options.session.agent_display_name
    )
}

/// Extract a one-line commit message from the agent's transcript file.
///
/// Reads the transcript JSONL, condenses it into structured entries, then
/// looks for assistant text that appears **after the last tool call**. This
/// is the wrap-up summary ("I've set up the project..."), not the opening
/// analysis ("This appears to be a minimal repository...").
///
/// If there is no assistant text after the last tool call, returns `None`.
/// This is common — Claude often ends with a tool call and no final message.
///
/// This is purely a file read. No subprocess, no API call, no network.
fn summarize_from_transcript(transcript_path: &std::path::Path) -> Option<String> {
    // Read the transcript file (best-effort, non-fatal)
    let raw = match std::fs::read(transcript_path) {
        Ok(data) if !data.is_empty() => data,
        Ok(_) => {
            log::debug!("Transcript file is empty");
            return None;
        }
        Err(e) => {
            log::debug!("Could not read transcript for summarization: {}", e);
            return None;
        }
    };

    // Parse into condensed entries using the existing transcript parser
    let entries = transcript::condense_claude_transcript(&raw);
    if entries.is_empty() {
        return None;
    }

    // Find the index of the last tool call in the transcript.
    // We only want assistant text that comes AFTER this point — that's
    // the wrap-up summary. Text before tool calls is Claude planning,
    // analyzing, or describing what it's about to do (not what it did).
    let last_tool_idx = entries.iter().rposition(|e| e.is_tool())?; // If no tool calls, no useful summary

    // Find the first assistant text entry AFTER the last tool call.
    // Claude's wrap-up message typically comes right after the final tool use.
    let wrap_up = entries[last_tool_idx + 1..]
        .iter()
        .find(|e| e.is_assistant() && e.content.is_some())?;

    let text = wrap_up.content.as_deref()?;
    let text = text.trim();

    if text.is_empty() {
        return None;
    }

    // Extract the first sentence. Claude's summaries often start with:
    //   "I've set up the project with TypeScript, Express, and tests."
    //   "The authentication bug has been fixed in login.rs."
    //   "Here's what I did:\n\n1. Created..."
    //
    // We want just that first sentence as a commit message.
    let summary = extract_first_sentence(text);

    // Skip if the extracted sentence is too short to be useful,
    // or if it looks like a question/filler rather than a summary.
    if summary.len() <= 10 {
        return None;
    }

    Some(summary)
}

/// Extract the first sentence from a block of text.
///
/// Splits on sentence-ending punctuation (`.`, `!`, `?`) followed by
/// whitespace or end-of-string. Also splits on `\n\n` (paragraph break)
/// and `:` followed by a newline (list introductions like "Here's what I did:").
///
/// Returns the full text if no sentence boundary is found.
fn extract_first_sentence(text: &str) -> String {
    // Collapse leading whitespace / blank lines
    let text = text.trim_start();

    // Split on paragraph break FIRST — this scopes all subsequent checks
    // to the first paragraph only, preventing cross-paragraph matches
    // (e.g., a ":\n" in the second paragraph shouldn't affect the first).
    let first_para = if let Some(idx) = text.find("\n\n") {
        let para = text[..idx].trim();
        if para.len() > 10 {
            para
        } else {
            text
        }
    } else {
        text
    };

    // Check for colon-then-newline pattern ("Here's what I did:\n")
    // within the first paragraph — take the part before the colon
    if let Some(idx) = first_para.find(":\n") {
        let before = first_para[..idx].trim();
        if before.len() > 10 {
            return before.to_string();
        }
    }

    // If the first paragraph ends with a colon (list introduction that
    // was split at \n\n), strip the colon and use the rest as the summary.
    let first_para = first_para.strip_suffix(':').unwrap_or(first_para);

    extract_first_sentence_from_paragraph(first_para)
}

/// Common abbreviations that end with a period but aren't sentence endings.
const ABBREVIATIONS: &[&str] = &[
    "e.g.", "i.e.", "vs.", "etc.", "Dr.", "Mr.", "Mrs.", "Ms.", "Jr.", "Sr.", "St.", "Ave.",
    "Blvd.", "approx.", "dept.", "est.", "govt.",
];

/// Check whether a period at `pos` in `text` is part of a known abbreviation.
///
/// Requires a word boundary before the abbreviation — the abbreviation must
/// be preceded by a space, start of string, or punctuation. This prevents
/// "Jest." from matching "est." or "arest." from matching "est.".
fn is_abbreviation(text: &str, dot_pos: usize) -> bool {
    let before = &text[..=dot_pos];
    for abbr in ABBREVIATIONS {
        if before.ends_with(abbr) {
            // Check word boundary: the character before the abbreviation
            // must be whitespace, start-of-string, or non-alphabetic.
            let abbr_start = before.len() - abbr.len();
            if abbr_start == 0 {
                // Abbreviation is at the very start of the text
                return true;
            }
            let preceding = before.as_bytes()[abbr_start - 1];
            if !preceding.is_ascii_alphabetic() {
                // Preceded by space, punctuation, etc. — real abbreviation
                return true;
            }
            // Otherwise it's a suffix match (e.g., "Jest." matching "est.")
            // — not an abbreviation, keep looking
        }
    }

    // Also catch single-letter abbreviations like "e." in "e.g."
    // by checking if the character before the dot is a single letter
    // preceded by another dot (pattern: letter-dot-letter-dot)
    if dot_pos >= 2 {
        let bytes = text.as_bytes();
        if bytes[dot_pos - 1].is_ascii_alphabetic() && bytes[dot_pos - 2] == b'.' {
            return true;
        }
    }

    false
}

/// Extract the first sentence from a single paragraph of text.
///
/// Looks for sentence-ending punctuation followed by whitespace or EOF.
/// Skips known abbreviations like "e.g.", "i.e.", "vs.", etc.
fn extract_first_sentence_from_paragraph(text: &str) -> String {
    let bytes = text.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        if b == b'.' || b == b'!' || b == b'?' {
            // Check if this looks like end-of-sentence:
            // - end of string
            // - followed by whitespace
            let at_end = i + 1 >= bytes.len();
            let followed_by_space = !at_end && (bytes[i + 1] == b' ' || bytes[i + 1] == b'\n');

            if at_end || followed_by_space {
                // Skip known abbreviations (e.g., i.e., vs., etc.)
                if b == b'.' && is_abbreviation(text, i) {
                    continue;
                }

                // Include the punctuation mark
                let sentence = text[..=i].trim();
                // Skip very short matches (probably abbreviations we missed)
                if sentence.len() > 10 {
                    return sentence.to_string();
                }
            }
        }
    }

    // No sentence boundary found — return the whole thing
    text.trim().to_string()
}

/// Check whether a prompt is meaningful enough to use as a change message.
///
/// Returns `false` for:
/// - Slash commands (`/init`, `/help`, `/review`, etc.)
/// - Very short prompts (≤ 3 chars) that are likely typos or noise
/// - Empty or whitespace-only prompts
fn is_meaningful_prompt(prompt: &str) -> bool {
    let trimmed = prompt.trim();

    // Empty / whitespace-only
    if trimmed.is_empty() {
        return false;
    }

    // Slash commands — these describe the tool invocation, not the intent
    if trimmed.starts_with('/') {
        return false;
    }

    // Very short prompts are unlikely to be descriptive
    if trimmed.len() <= 3 {
        return false;
    }

    true
}

/// Build a human-readable summary of file changes from the repository status.
///
/// Groups changes by kind and lists filenames (not full paths) to keep the
/// message concise. Examples:
///
/// - `"Add src/main.rs, Cargo.toml"`
/// - `"Modify auth.rs; delete old_config.toml"`
/// - `"Add 12 files"`
/// - `"Modify handler.rs; add 5 files"`
fn build_file_change_summary(status: &RepositoryStatus, untracked_paths: &[String]) -> String {
    let mut added: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();

    for entry in status.entries() {
        let filename = entry
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| entry.path().to_string_lossy().to_string());

        match entry.status() {
            FileStatus::Added => added.push(filename),
            FileStatus::Modified => modified.push(filename),
            FileStatus::Deleted => deleted.push(filename),
            _ => {}
        }
    }

    // Untracked files that will be auto-added count as "Add"
    for path in untracked_paths {
        let filename = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        added.push(filename);
    }

    let mut parts: Vec<String> = Vec::new();

    if !modified.is_empty() {
        parts.push(format_file_group("Modify", &modified));
    }
    if !added.is_empty() {
        parts.push(format_file_group("Add", &added));
    }
    if !deleted.is_empty() {
        parts.push(format_file_group("Delete", &deleted));
    }

    parts.join("; ")
}

/// Format a group of files with a verb prefix.
///
/// - 1-3 files: `"Add src/main.rs, Cargo.toml, lib.rs"`
/// - 4+ files:  `"Add src/main.rs, Cargo.toml (+2 more)"`
fn format_file_group(verb: &str, files: &[String]) -> String {
    match files.len() {
        0 => String::new(),
        1 => format!("{} {}", verb, files[0]),
        2 => format!("{} {}, {}", verb, files[0], files[1]),
        3 => format!("{} {}, {}, {}", verb, files[0], files[1], files[2]),
        n => format!("{} {}, {} (+{} more)", verb, files[0], files[1], n - 2),
    }
}

/// Truncate a prompt to the given maximum length, adding "..." if needed.
fn truncate_prompt(prompt: &str, max_len: usize) -> String {
    let trimmed = prompt.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(max_len.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}

// Provenance Construction

/// Build a `Provenance` entry for an agent turn.
///
/// Populates vendor, model, tool, suggestion type, session ID, and prompt hash.
/// Token usage and cost can be added later when that data is available from
/// the agent's transcript.
fn build_turn_provenance(options: &TurnRecordOptions<'_>) -> Provenance {
    let vendor = if options.session.agent_vendor.is_empty() {
        vendor_from_agent_name(&options.session.agent_name)
    } else {
        AIVendor::parse(&options.session.agent_vendor)
    };

    let model = if options.session.model.is_empty() {
        "unknown".to_string()
    } else {
        options.session.model.clone()
    };

    let tool = AITool::Cli(options.session.agent_name.clone());

    let prompt_content = match &options.prompt {
        Some(prompt) if !prompt.is_empty() => PromptContent::Hashed(Hash::of(prompt.as_bytes())),
        _ => PromptContent::None,
    };

    let timestamp = options.event.timestamp.timestamp();

    // Extract enriched metadata from the raw JSON payload sent by the plugin.
    // The plugin accumulates data across all events within a turn and sends
    // it in the `stop` payload. All fields are optional — old plugins that
    // don't send them will simply leave these as None/default.
    let raw = options.event.raw_json.as_ref();

    // Helper closures for extracting typed values from raw JSON
    let raw_str = |key: &str| -> Option<String> {
        raw.and_then(|r| r.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let raw_u64 =
        |key: &str| -> Option<u64> { raw.and_then(|r| r.get(key)).and_then(|v| v.as_u64()) };
    let raw_f64 =
        |key: &str| -> Option<f64> { raw.and_then(|r| r.get(key)).and_then(|v| v.as_f64()) };
    let raw_u32 = |key: &str| -> Option<u32> {
        raw.and_then(|r| r.get(key))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
    };

    // Token usage — now includes reasoning tokens
    let input = raw_u64("input_tokens").unwrap_or(0);
    let output = raw_u64("output_tokens").unwrap_or(0);
    let reasoning = raw_u64("reasoning_tokens").unwrap_or(0);
    let cache_read = raw_u64("cache_read_tokens").unwrap_or(0);
    let cache_write = raw_u64("cache_write_tokens").unwrap_or(0);

    let tokens = if input > 0 || output > 0 || reasoning > 0 || cache_read > 0 || cache_write > 0 {
        TokenUsage::full(input, output, reasoning, cache_read, cache_write)
    } else {
        TokenUsage::default()
    };

    // Cost
    let cost = match raw_f64("cost_usd") {
        Some(usd) if usd > 0.0 => Cost::from_usd(usd),
        _ => Cost::zero(),
    };

    // Agent mode — "build", "code", "ask"
    let agent_mode = raw_str("agent");

    // Finish reason — "stop", "tool-calls", "length"
    let finish_reason = raw_str("finish_reason");

    // Step count — number of LLM roundtrips in this turn
    let step_count = raw_u32("step_count");

    // Session slug — human-readable name like "mighty-rocket"
    let session_slug = raw_str("session_slug");

    // Reasoning signature — Anthropic cryptographic proof
    let reasoning_signature = raw_str("reasoning_signature");

    // Reasoning text — concatenated chain-of-thought, truncated to 10KB
    let reasoning_text = raw_str("reasoning_text").map(|t| {
        if t.len() > 10_240 {
            format!("{}...[truncated, {} chars total]", &t[..10_240], t.len())
        } else {
            t
        }
    });

    // Task plan — agent's todo list serialized as JSON string
    let task_plan = raw
        .and_then(|r| r.get("todos"))
        .and_then(|v| serde_json::to_string(v).ok());

    // Build metadata key-value pairs
    let mut metadata = vec![
        ("turn_number".to_string(), options.turn_number.to_string()),
        ("agent_name".to_string(), options.session.agent_name.clone()),
    ];
    if let Some(ref mode) = agent_mode {
        metadata.push(("agent_mode".to_string(), mode.clone()));
    }
    if let Some(ref reason) = finish_reason {
        metadata.push(("finish_reason".to_string(), reason.clone()));
    }
    if let Some(steps) = step_count {
        metadata.push(("step_count".to_string(), steps.to_string()));
    }

    Provenance {
        vendor,
        model,
        model_version: None,
        tool,
        suggestion_type: SuggestionType::Complete,
        prompt: prompt_content,
        system_prompt_hash: None,
        tokens,
        cost,
        temperature: None,
        timestamp: Some(timestamp),
        request_id: None,
        session_id: Some(options.session.session_id.clone()),
        metadata,
        agent_mode,
        finish_reason,
        step_count,
        session_slug,
        reasoning_signature,
        reasoning_text,
        task_plan,
    }
}

/// Infer the AI vendor from the agent name.
///
/// Used as a fallback when `session.agent_vendor` is not set.
fn vendor_from_agent_name(agent_name: &str) -> AIVendor {
    match agent_name {
        "claude-code" => AIVendor::Anthropic,
        "gemini-cli" => AIVendor::Google,
        "codex" => AIVendor::OpenAI,
        _ => AIVendor::Other(agent_name.to_string()),
    }
}

// SessionEnvelope Construction

/// Build a `SessionEnvelope` for embedding in `HashedChange.metadata`.
fn build_turn_envelope(
    options: &TurnRecordOptions<'_>,
    recorded_files: &[String],
) -> SessionEnvelope {
    let prompt_summary = options
        .prompt
        .as_deref()
        .filter(|p| is_meaningful_prompt(p))
        .map(|p| truncate_prompt(p, 200));

    let prompt_hash = options
        .prompt
        .as_deref()
        .filter(|p| is_meaningful_prompt(p))
        .map(|p| *blake3::hash(p.as_bytes()).as_bytes());

    let files_in_turn: Vec<String> = recorded_files.to_vec();

    let session_started_at = options.session.started_at.timestamp();
    let turn_ended_at = options.event.timestamp.timestamp();
    let turn_started_at = turn_ended_at - (options.turn_duration_ms as i64 / 1000);

    let mut builder =
        SessionEnvelope::builder(&options.session.session_id, &options.session.agent_name)
            .agent_display_name(&options.session.agent_display_name)
            .turn_number(options.turn_number)
            .session_started_at(session_started_at)
            .turn_started_at(turn_started_at)
            .turn_ended_at(turn_ended_at)
            .turn_duration_ms(options.turn_duration_ms)
            .files_in_turn(files_in_turn)
            .files_in_session(options.session.files_touched_count());

    if let Some(summary) = prompt_summary {
        builder = builder.prompt_summary(summary);
    }
    if let Some(hash) = prompt_hash {
        builder = builder.prompt_hash(hash);
    }

    builder.build()
}

// record_turn (the main entry point)

/// Record an agent turn as an Atomic change.
///
/// This is the function that bridges the agent world into the VCS world.
/// It builds a `ChangeHeader`, `Provenance`, and `SessionEnvelope`, then
/// calls the repository's `record()` method to create a proper content-addressed,
/// hashable, pushable Atomic change.
///
/// # Arguments
///
/// * `repo_root` — Path to the repository root (where `.atomic/` lives).
///   The repository is opened fresh for each recording to avoid stale state.
/// * `options` — Turn recording options (session, changes, event, turn number)
///
/// # Returns
///
/// A `TurnRecordOutcome` with the change hash, turn number, file count,
/// and message. The change has already been applied to the agent's view.
///
/// # Errors
///
/// Returns `AgentError::EmptyTurn` if the repository has nothing to record
/// (no files changed since the last recorded state).
/// Returns `AgentError::RecordFailed` if the repository record operation fails.
///
/// # What Goes Into The Change
///
/// ```text
/// HashedChange {
///     header: ChangeHeader {
///         message: "Turn 3: Fix the authentication bug...",
///         authors: [Author { name: "Claude Code" }],
///         timestamp: <turn end time>,
///     },
///     provenance: [Provenance {
///         vendor: Anthropic,
///         model: "claude-sonnet-4-20250514",
///         tool: Cli("claude-code"),
///         session_id: Some("sess-abc-123"),
///         prompt: Hashed(<blake3>),
///         metadata: [("turn_number", "3"), ("agent_name", "claude-code")],
///     }],
///     metadata: <SessionEnvelope postcard bytes>,  // ← tamper-evident session data
///     hunks: [...],        // ← actual file diffs from repo.record()
///     file_ops: [...],     // ← CRDT semantic ops from repo.record()
///     contents: [...],     // ← file content from repo.record()
/// }
/// ```
pub fn record_turn(
    repo_root: &Path,
    options: &TurnRecordOptions<'_>,
) -> AgentResult<TurnRecordOutcome> {
    // Step 1: Open the repository
    let repo =
        atomic_repository::Repository::open(repo_root).map_err(|e| AgentError::RecordFailed {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
            reason: format!("Failed to open repository: {}", e),
        })?;

    // Step 2: Status — find out what the agent changed
    let status = repo
        .status(atomic_repository::status::StatusOptions::default())
        .map_err(|e| AgentError::RecordFailed {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
            reason: format!("Failed to get repository status: {}", e),
        })?;

    // Check if there's anything to record at all
    if status.is_clean() && status.untracked_count() == 0 {
        return Err(AgentError::EmptyTurn {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
        });
    }

    // Step 3: Add — track any new files the agent created
    // Agents create new files all the time (new modules, tests, configs).
    // These show up as "untracked" in status. We add them before recording
    // so they're included in the change.
    //
    // We filter out common large directories (node_modules, target, etc.)
    // that agents may create as side effects (e.g., `npm install`). These
    // would make the hook extremely slow and are never intended to be
    // version-controlled. The .atomicignore file provides user-level control,
    // but these defaults protect against the common case where no ignore
    // file exists yet.
    let untracked_paths: Vec<String> = status
        .untracked()
        .map(|e| e.path().to_string_lossy().to_string())
        .filter(|p| !should_ignore_untracked(p))
        .collect();

    if !untracked_paths.is_empty() {
        log::info!(
            "Adding {} untracked file{} created by agent",
            untracked_paths.len(),
            if untracked_paths.len() == 1 { "" } else { "s" },
        );

        let tracking_options = atomic_repository::tracking::TrackingOptions::default();
        for path in &untracked_paths {
            if let Err(e) = repo.add(path, tracking_options.clone()) {
                log::warn!("Failed to add '{}': {} (skipping)", path, e);
            }
        }
    }

    // Step 4: Build SessionEnvelope + Record the Atomic change
    // Build the header AFTER status so the message can describe actual changes
    // instead of parroting slash commands like "/init".
    let header = build_turn_header(options, &status, &untracked_paths);
    let provenance = build_turn_provenance(options);
    let message = build_turn_message(options, &status, &untracked_paths);

    // Build the SessionEnvelope BEFORE recording so it can be included in
    // the change hash via RecordOptions::metadata_bytes(). We use the files
    // from status (what we're about to record) rather than waiting for the
    // outcome — they're the same set since we use `all: true`.
    let status_files: Vec<String> = status
        .entries()
        .iter()
        .filter(|e| e.status().is_dirty())
        .map(|e| e.path().to_string_lossy().to_string())
        .chain(untracked_paths.iter().cloned())
        .collect();

    let envelope = build_turn_envelope(options, &status_files);
    let envelope_bytes = envelope.encode().map_err(|e| AgentError::RecordFailed {
        session_id: options.session.session_id.clone(),
        turn_number: options.turn_number,
        reason: format!("Failed to encode SessionEnvelope: {}", e),
    })?;

    // Record all dirty files. The SessionEnvelope bytes are included in
    // HashedChange.metadata — part of the change's cryptographic identity.
    // This means session structure (turn number, timing, files, agent name)
    // is tamper-evident and commutes via patch theory.
    let record_options = atomic_repository::record::RecordOptions::new()
        .with_all(true)
        .view(options.session.view_name.clone())
        .apply_after_record(true)
        .save_to_store(true)
        .provenance(vec![provenance])
        .metadata_bytes(envelope_bytes);

    let mut outcome = match repo.record(header, record_options) {
        Ok(outcome) => outcome,
        Err(atomic_repository::record::RecordError::NothingToRecord) => {
            return Err(AgentError::EmptyTurn {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
            });
        }
        Err(e) => {
            return Err(AgentError::RecordFailed {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
                reason: format!("Record failed: {}", e),
            });
        }
    };

    // Step 5: Collect results
    let recorded_files: Vec<String> = outcome
        .recorded_files()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let file_count = recorded_files.len();

    // Step 6: Condense transcript + generate reasoning + attach to unhashed
    //
    // Read the agent's transcript file, condense it into structured entries,
    // optionally generate an AI reasoning summary, anchor code learnings to
    // the CRDT graph, then attach everything to the change's unhashed section.
    //
    // All of this is non-fatal — if any step fails, the change is still valid.
    // The unhashed data is also stored in TurnRecordOutcome so the orchestrator
    // can log/display it.

    let unhashed_data = build_unhashed_turn_data(options, &recorded_files, &outcome);

    if let Some(ref data) = unhashed_data {
        match transcript::attach_unhashed(outcome.change_mut(), data) {
            Ok(()) => {
                log::info!(
                    "Attached transcript ({} entries{}) to change for turn {}",
                    data.entry_count(),
                    if data.has_reasoning() {
                        " + reasoning"
                    } else {
                        ""
                    },
                    options.turn_number,
                );
            }
            Err(e) => {
                log::warn!(
                    "Failed to attach unhashed data to change (non-fatal): {}",
                    e
                );
            }
        }
    }

    let hash = *outcome.hash();

    Ok(TurnRecordOutcome {
        hash,
        turn_number: options.turn_number,
        file_count,
        message,
        recorded_files,
        unhashed_data,
    })
}

// Step 6 Helper: Build Unhashed Turn Data

/// Build the unhashed turn data (transcript + reasoning) from the agent's
/// transcript file and the recorded change's FileOps.
///
/// Returns `None` if the transcript is not available, empty, or unparseable.
/// Reasoning generation failure is logged and skipped (non-fatal).
fn build_unhashed_turn_data(
    options: &TurnRecordOptions<'_>,
    recorded_files: &[String],
    _outcome: &atomic_repository::record::RecordOutcome,
) -> Option<transcript::UnhashedTurnData> {
    // Check if we have a transcript file
    let transcript_path = options.session.transcript_path.as_ref()?;
    if !transcript_path.exists() {
        log::debug!(
            "Transcript file not found at {} — skipping",
            transcript_path.display(),
        );
        return None;
    }

    // Read raw transcript
    let raw = match std::fs::read(transcript_path) {
        Ok(data) => data,
        Err(e) => {
            log::warn!(
                "Failed to read transcript at {}: {}",
                transcript_path.display(),
                e
            );
            return None;
        }
    };

    if raw.is_empty() {
        return None;
    }

    // Determine format from agent name
    let format = if options.session.agent_name.contains("gemini") {
        "json"
    } else {
        "jsonl"
    };

    // Condense into structured entries
    let entries = transcript::condense_transcript(&raw, format);
    if entries.is_empty() {
        log::debug!("Condensed transcript is empty — skipping");
        return None;
    }

    // Build the base unhashed data
    let data = transcript::UnhashedTurnData::new(
        &options.session.session_id,
        options.turn_number,
        format,
        entries,
        recorded_files,
    );

    // Reasoning generation is NOT done here — it's too slow for the hook
    // hot path (calls Claude CLI, 30+ seconds) and potentially recursive
    // when running inside a Claude Code hook.
    //
    // Use `atomic agent explain` to generate reasoning on demand after
    // the turn is recorded. That command reads the change's unhashed
    // transcript, calls Claude CLI, anchors learnings to the CRDT graph,
    // and updates the change.

    Some(data)
}

// Ignore Patterns for Untracked Files

/// Directories and patterns that should never be auto-added by agent recording.
///
/// These are common large directories that agents may create as side effects
/// (e.g., running `npm install`, `cargo build`, `pip install`). Walking them
/// makes the hook extremely slow and they're never intended to be tracked.
///
/// Users can override this with `.atomicignore` for fine-grained control.
const AUTO_ADD_IGNORE_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".claude",
    ".gemini",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".env",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".cache",
    ".parcel-cache",
    "coverage",
    ".nyc_output",
    ".pytest_cache",
    ".mypy_cache",
    ".tox",
    "vendor",
    "Pods",
    ".gradle",
    ".idea",
    ".vscode",
    ".DS_Store",
];

/// Check if an untracked file path should be ignored during auto-add.
///
/// Returns `true` if any path component matches one of the known large
/// directories that should never be auto-tracked.
fn should_ignore_untracked(path: &str) -> bool {
    for component in std::path::Path::new(path).components() {
        if let std::path::Component::Normal(name) = component {
            let name_str = name.to_string_lossy();
            if AUTO_ADD_IGNORE_DIRS.contains(&name_str.as_ref()) {
                return true;
            }
            // Also skip hidden directories (starting with .) other than
            // specific ones we already check above
            if name_str.starts_with('.') && name_str.len() > 1 {
                return true;
            }
        }
    }
    false
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{HookType, TurnEvent};
    use crate::turn::session::AgentSession;

    fn make_session() -> AgentSession {
        let mut s = AgentSession::new("sess-test-123", "claude-code", "Claude Code");
        s.set_model_info("anthropic", "claude-sonnet-4-20250514");
        s.turn_count = 2; // Already completed 2 turns
        s.add_files_touched(&["src/main.rs".to_string(), "src/lib.rs".to_string()]);
        s
    }

    fn make_event() -> TurnEvent {
        TurnEvent::new("sess-test-123", HookType::TurnEnd)
    }

    fn make_options<'a>(session: &'a AgentSession, event: &'a TurnEvent) -> TurnRecordOptions<'a> {
        TurnRecordOptions {
            session,
            event,
            turn_number: 3,
            turn_duration_ms: 12400,
            prompt: Some("Fix the authentication bug in login.rs".to_string()),
        }
    }

    // truncate_prompt tests

    #[test]
    fn test_truncate_prompt_short() {
        assert_eq!(truncate_prompt("hello", 72), "hello");
    }

    #[test]
    fn test_truncate_prompt_exact() {
        let s = "a".repeat(72);
        assert_eq!(truncate_prompt(&s, 72), s);
    }

    #[test]
    fn test_truncate_prompt_long() {
        let s = "a".repeat(100);
        let result = truncate_prompt(&s, 72);
        assert!(result.len() <= 72);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_prompt_trims_whitespace() {
        assert_eq!(truncate_prompt("  hello  ", 72), "hello");
    }

    #[test]
    fn test_truncate_prompt_unicode() {
        let s = "修复".repeat(50);
        let result = truncate_prompt(&s, 20);
        assert!(result.ends_with("..."));
        assert!(result.chars().count() <= 20);
    }

    // build_turn_message tests

    fn empty_status() -> RepositoryStatus {
        RepositoryStatus::new("main".to_string(), None)
    }

    fn no_untracked() -> Vec<String> {
        vec![]
    }

    // is_meaningful_prompt tests

    #[test]
    fn test_slash_command_not_meaningful() {
        assert!(!is_meaningful_prompt("/init"));
        assert!(!is_meaningful_prompt("/help"));
        assert!(!is_meaningful_prompt("/review"));
        assert!(!is_meaningful_prompt("/compact"));
    }

    #[test]
    fn test_empty_prompt_not_meaningful() {
        assert!(!is_meaningful_prompt(""));
        assert!(!is_meaningful_prompt("   "));
    }

    #[test]
    fn test_very_short_prompt_not_meaningful() {
        assert!(!is_meaningful_prompt("hi"));
        assert!(!is_meaningful_prompt("ok"));
        assert!(!is_meaningful_prompt("y"));
    }

    #[test]
    fn test_descriptive_prompt_is_meaningful() {
        assert!(is_meaningful_prompt(
            "Fix the authentication bug in login.rs"
        ));
        assert!(is_meaningful_prompt("Add unit tests for the parser module"));
        assert!(is_meaningful_prompt("refactor error handling"));
    }

    // format_file_group tests

    #[test]
    fn test_format_file_group_single() {
        assert_eq!(
            format_file_group("Add", &["main.rs".to_string()]),
            "Add main.rs"
        );
    }

    #[test]
    fn test_format_file_group_two() {
        assert_eq!(
            format_file_group("Modify", &["auth.rs".to_string(), "lib.rs".to_string()]),
            "Modify auth.rs, lib.rs"
        );
    }

    #[test]
    fn test_format_file_group_three() {
        assert_eq!(
            format_file_group(
                "Delete",
                &["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()]
            ),
            "Delete a.rs, b.rs, c.rs"
        );
    }

    #[test]
    fn test_format_file_group_many() {
        let files: Vec<String> = (0..6).map(|i| format!("file{}.rs", i)).collect();
        assert_eq!(
            format_file_group("Add", &files),
            "Add file0.rs, file1.rs (+4 more)"
        );
    }

    #[test]
    fn test_format_file_group_empty() {
        assert_eq!(format_file_group("Add", &[]), "");
    }

    // build_turn_message tests

    // build_turn_message priority tests

    #[test]
    fn test_message_prompt_beats_file_summary() {
        // Meaningful prompt should win over file summary
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);
        let status = empty_status();
        let untracked = vec!["src/main.rs".to_string()];

        // Has both a meaningful prompt AND untracked files
        let msg = build_turn_message(&options, &status, &untracked);
        // Prompt wins
        assert_eq!(msg, "Fix the authentication bug in login.rs");
    }

    #[test]
    fn test_message_file_summary_beats_transcript() {
        // File summary should beat transcript when prompt is a slash command
        let mut session = make_session();
        let event = make_event();

        // Write a transcript with assistant text BEFORE tool calls
        // (planning text, not summary)
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("transcript.jsonl");
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"This appears to be a minimal repository managed by Atomic."}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"CLAUDE.md"}}]}}"#,
        ];
        std::fs::write(&transcript_path, lines.join("\n")).unwrap();
        session.transcript_path = Some(transcript_path);

        // Create options AFTER setting transcript_path (borrow checker)
        let mut options = make_options(&session, &event);
        options.prompt = Some("/init".to_string());

        let status = empty_status();
        let untracked = vec!["CLAUDE.md".to_string()];

        let msg = build_turn_message(&options, &status, &untracked);
        // File summary wins, NOT the transcript planning text
        assert_eq!(msg, "Add CLAUDE.md");
    }

    // extract_first_sentence / extract_first_sentence_from_paragraph tests

    #[test]
    fn test_extract_first_sentence_simple() {
        assert_eq!(
            extract_first_sentence("I've fixed the authentication bug. The tests now pass."),
            "I've fixed the authentication bug."
        );
    }

    #[test]
    fn test_extract_first_sentence_exclamation() {
        assert_eq!(
            extract_first_sentence("Done! Created the TypeScript project with all configs."),
            "Done! Created the TypeScript project with all configs."
        );
    }

    #[test]
    fn test_extract_first_sentence_colon_newline() {
        assert_eq!(
            extract_first_sentence(
                "Here's what I've set up for the project:\n\n1. TypeScript\n2. ESLint"
            ),
            "Here's what I've set up for the project"
        );
    }

    #[test]
    fn test_extract_first_sentence_colon_only_paragraph() {
        // When the entire first paragraph is a list introduction ending
        // with a colon, strip the colon for a cleaner message
        assert_eq!(
            extract_first_sentence("Changes made:\n\n- Fixed auth\n- Updated tests"),
            "Changes made"
        );
    }

    #[test]
    fn test_extract_first_sentence_paragraph_break() {
        // First paragraph has a clean sentence ending — should extract it
        assert_eq!(
            extract_first_sentence("Fixed the authentication bug in the login handler.\n\nThe change updates token validation."),
            "Fixed the authentication bug in the login handler."
        );
    }

    #[test]
    fn test_extract_first_sentence_paragraph_with_colon() {
        // Colon-newline in second paragraph should NOT interfere
        // with first-paragraph extraction
        assert_eq!(
            extract_first_sentence("Set up the TypeScript project with all dependencies.\n\nThe project includes:\n- src/index.ts"),
            "Set up the TypeScript project with all dependencies."
        );
    }

    #[test]
    fn test_extract_first_sentence_no_boundary() {
        assert_eq!(
            extract_first_sentence("Set up TypeScript project with Express and Jest"),
            "Set up TypeScript project with Express and Jest"
        );
    }

    #[test]
    fn test_extract_first_sentence_skips_abbreviations() {
        // "e.g." has dots but shouldn't split the sentence
        let text = "Fixed the config e.g. the timeout value was wrong. Also updated tests.";
        let result = extract_first_sentence(text);
        assert_eq!(result, "Fixed the config e.g. the timeout value was wrong.");
    }

    #[test]
    fn test_extract_first_sentence_skips_ie() {
        let text = "Updated the parser i.e. the tokenizer module. Tests pass.";
        let result = extract_first_sentence(text);
        assert_eq!(result, "Updated the parser i.e. the tokenizer module.");
    }

    #[test]
    fn test_extract_first_sentence_with_file_extensions() {
        // File extensions like ".ts" and ".json" have dots but aren't
        // sentence endings because they're followed by "," not " "
        let text =
            "Created src/index.ts, package.json, and tsconfig.json for the project. Tests pass.";
        let result = extract_first_sentence(text);
        assert_eq!(
            result,
            "Created src/index.ts, package.json, and tsconfig.json for the project."
        );
    }

    #[test]
    fn test_extract_paragraph_directly() {
        // Direct test of extract_first_sentence_from_paragraph
        let text = "I have created a hello world TypeScript project with Express and Jest. The project includes index.ts and package.json.";
        let result = extract_first_sentence_from_paragraph(text);
        assert_eq!(
            result,
            "I have created a hello world TypeScript project with Express and Jest."
        );
    }

    #[test]
    fn test_is_abbreviation_eg() {
        assert!(is_abbreviation("e.g.", 3));
        assert!(is_abbreviation("something e.g.", 13));
    }

    #[test]
    fn test_is_abbreviation_normal_word() {
        assert!(!is_abbreviation("bug.", 3));
        assert!(!is_abbreviation("the fix is done.", 15));
    }

    #[test]
    fn test_extract_first_sentence_skips_short_colon() {
        // Short text before colon shouldn't be used
        assert_eq!(
            extract_first_sentence("OK:\n- did stuff"),
            "OK:\n- did stuff"
        );
    }

    // summarize_from_transcript tests

    #[test]
    fn test_summarize_from_transcript_missing_file() {
        let path = std::path::Path::new("/tmp/nonexistent-atomic-test-transcript.jsonl");
        assert_eq!(summarize_from_transcript(path), None);
    }

    #[test]
    fn test_summarize_from_transcript_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(summarize_from_transcript(&path), None);
    }

    #[test]
    fn test_summarize_skips_text_before_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");

        // Claude analyzes BEFORE tool calls, then writes a file.
        // The pre-tool text should NOT be used as a commit message.
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"This appears to be a minimal repository managed by Atomic."}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"CLAUDE.md"}}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        // No assistant text AFTER the tool call → returns None
        assert_eq!(summarize_from_transcript(&path), None);
    }

    #[test]
    fn test_summarize_uses_text_after_last_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");

        // Claude plans, uses tools, then summarizes what it did.
        // Only the post-tool summary should be used.
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Let me analyze the codebase first."}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/main.rs"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/auth.rs"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I have fixed the authentication bug in the login handler. The token validation now checks expiry correctly."}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let result = summarize_from_transcript(&path).unwrap();
        assert_eq!(
            result,
            "I have fixed the authentication bug in the login handler."
        );
    }

    #[test]
    fn test_summarize_no_tool_calls_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");

        // Only assistant text, no tool calls — can't distinguish
        // planning from summary, so return None.
        let lines = vec![
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"fix it"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I will look into this."}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        assert_eq!(summarize_from_transcript(&path), None);
    }

    #[test]
    fn test_summarize_with_file_extensions_after_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");

        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"src/index.ts"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Created src/index.ts, package.json, and tsconfig.json for the project. All dependencies are installed."}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let result = summarize_from_transcript(&path).unwrap();
        assert_eq!(
            result,
            "Created src/index.ts, package.json, and tsconfig.json for the project."
        );
    }

    // build_turn_message tests (integration with transcript)

    #[test]
    fn test_message_transcript_used_when_prompt_and_files_unavailable() {
        let mut session = make_session();
        let event = make_event();

        // Write a transcript with a post-tool summary
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("transcript.jsonl");
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"README.md"}}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Created the project README with setup instructions. The documentation covers installation and usage."}]}}"#,
        ];
        std::fs::write(&transcript_path, lines.join("\n")).unwrap();
        session.transcript_path = Some(transcript_path);

        // Create options AFTER setting transcript_path (borrow checker)
        let mut options = make_options(&session, &event);
        options.prompt = Some("/init".to_string()); // slash command, not meaningful

        let status = empty_status();

        // No untracked files and no dirty files → file summary is empty
        // → transcript is used as priority 3
        let msg = build_turn_message(&options, &status, &no_untracked());
        assert_eq!(msg, "Created the project README with setup instructions.");
    }

    #[test]
    fn test_message_falls_back_to_prompt_without_transcript() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);
        let status = empty_status();

        // No transcript_path on session → falls back to prompt
        let msg = build_turn_message(&options, &status, &no_untracked());
        assert_eq!(msg, "Fix the authentication bug in login.rs");
    }

    #[test]
    fn test_message_with_slash_command_falls_back_to_files() {
        let session = make_session();
        let event = make_event();
        let mut options = make_options(&session, &event);
        options.prompt = Some("/init".to_string());

        // With untracked files that will be auto-added
        let untracked = vec!["src/main.rs".to_string(), "Cargo.toml".to_string()];
        let status = empty_status();

        let msg = build_turn_message(&options, &status, &untracked);
        assert_eq!(msg, "Add main.rs, Cargo.toml");
    }

    #[test]
    fn test_message_without_prompt_uses_files() {
        let session = make_session();
        let event = make_event();
        let mut options = make_options(&session, &event);
        options.prompt = None;

        let untracked = vec!["src/lib.rs".to_string()];
        let status = empty_status();

        let msg = build_turn_message(&options, &status, &untracked);
        assert_eq!(msg, "Add lib.rs");
    }

    #[test]
    fn test_message_no_prompt_no_files_falls_back() {
        let session = make_session();
        let event = make_event();
        let mut options = make_options(&session, &event);
        options.prompt = None;

        let status = empty_status();
        let msg = build_turn_message(&options, &status, &no_untracked());
        assert_eq!(msg, "Turn 3 (Claude Code)");
    }

    #[test]
    fn test_message_with_long_prompt() {
        let session = make_session();
        let event = make_event();
        let mut options = make_options(&session, &event);
        options.prompt = Some("a".repeat(200));
        let status = empty_status();

        let msg = build_turn_message(&options, &status, &no_untracked());
        // The prompt is meaningful (long, not a slash command)
        assert!(msg.len() <= 72);
        assert!(msg.ends_with("..."));
    }

    // build_file_change_summary tests

    #[test]
    fn test_summary_untracked_only() {
        let status = empty_status();
        let untracked = vec![
            "src/main.rs".to_string(),
            "Cargo.toml".to_string(),
            "README.md".to_string(),
        ];
        let summary = build_file_change_summary(&status, &untracked);
        assert_eq!(summary, "Add main.rs, Cargo.toml, README.md");
    }

    #[test]
    fn test_summary_empty() {
        let status = empty_status();
        let summary = build_file_change_summary(&status, &[]);
        assert_eq!(summary, "");
    }

    // build_turn_header tests

    #[test]
    fn test_header_has_message() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let status = empty_status();
        let header = build_turn_header(&options, &status, &no_untracked());
        assert_eq!(header.message, "Fix the authentication bug in login.rs");
    }

    #[test]
    fn test_header_has_author() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let status = empty_status();
        let header = build_turn_header(&options, &status, &no_untracked());
        assert!(!header.authors.is_empty());
        // Author is either "claude+sess" (if user identity found in ~/.atomic/identities/)
        // or "Claude Code" (fallback when no identity configured).
        let name = &header.authors[0].name;
        assert!(
            name == "Claude Code" || name.starts_with("claude+"),
            "Expected 'Claude Code' or 'claude+...' but got: {}",
            name
        );
    }

    // vendor_from_agent_name tests

    #[test]
    fn test_vendor_from_agent_name_claude() {
        assert_eq!(vendor_from_agent_name("claude-code"), AIVendor::Anthropic);
    }

    #[test]
    fn test_vendor_from_agent_name_gemini() {
        assert_eq!(vendor_from_agent_name("gemini-cli"), AIVendor::Google);
    }

    #[test]
    fn test_vendor_from_agent_name_codex() {
        assert_eq!(vendor_from_agent_name("codex"), AIVendor::OpenAI);
    }

    #[test]
    fn test_vendor_from_agent_name_unknown() {
        match vendor_from_agent_name("my-custom-agent") {
            AIVendor::Other(name) => assert_eq!(name, "my-custom-agent"),
            other => panic!("Expected Other, got: {:?}", other),
        }
    }

    // build_turn_provenance tests

    #[test]
    fn test_provenance_vendor_and_model() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        assert_eq!(prov.vendor, AIVendor::Anthropic);
        assert_eq!(prov.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_provenance_tool() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        assert_eq!(prov.tool, AITool::Cli("claude-code".to_string()));
    }

    #[test]
    fn test_provenance_session_id() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        assert_eq!(prov.session_id, Some("sess-test-123".to_string()));
    }

    #[test]
    fn test_provenance_prompt_hash() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        assert!(prov.prompt.hash().is_some());
        // Should be a hash, not the full text (privacy)
        assert!(!prov.prompt.has_full_text());
    }

    #[test]
    fn test_provenance_no_prompt() {
        let session = make_session();
        let event = make_event();
        let mut options = make_options(&session, &event);
        options.prompt = None;

        let prov = build_turn_provenance(&options);
        assert!(matches!(prov.prompt, PromptContent::None));
    }

    #[test]
    fn test_provenance_suggestion_type_complete() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        assert_eq!(prov.suggestion_type, SuggestionType::Complete);
    }

    #[test]
    fn test_provenance_metadata_has_turn_number() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        let turn_meta = prov.metadata.iter().find(|(k, _)| k == "turn_number");
        assert!(turn_meta.is_some());
        assert_eq!(turn_meta.unwrap().1, "3");
    }

    #[test]
    fn test_provenance_metadata_has_agent_name() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        let agent_meta = prov.metadata.iter().find(|(k, _)| k == "agent_name");
        assert!(agent_meta.is_some());
        assert_eq!(agent_meta.unwrap().1, "claude-code");
    }

    #[test]
    fn test_provenance_vendor_fallback_from_agent_name() {
        let mut session = make_session();
        session.agent_vendor = String::new(); // Clear vendor
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        // Should infer Anthropic from "claude-code"
        assert_eq!(prov.vendor, AIVendor::Anthropic);
    }

    #[test]
    fn test_provenance_model_fallback_unknown() {
        let mut session = make_session();
        session.model = String::new(); // Clear model
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        assert_eq!(prov.model, "unknown");
    }

    #[test]
    fn test_provenance_has_timestamp() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let prov = build_turn_provenance(&options);
        assert!(prov.timestamp.is_some());
    }

    // build_turn_envelope tests

    #[test]
    fn test_envelope_session_id() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);
        let files = vec!["src/auth.rs".to_string()];

        let env = build_turn_envelope(&options, &files);
        assert_eq!(env.session_id, "sess-test-123");
    }

    #[test]
    fn test_envelope_agent_name() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);
        let files = vec!["src/auth.rs".to_string()];

        let env = build_turn_envelope(&options, &files);
        assert_eq!(env.agent_name, "claude-code");
        assert_eq!(env.agent_display_name.as_deref(), Some("Claude Code"));
    }

    #[test]
    fn test_envelope_turn_number() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let env = build_turn_envelope(&options, &[]);
        assert_eq!(env.turn_number, 3);
    }

    #[test]
    fn test_envelope_duration() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let env = build_turn_envelope(&options, &[]);
        assert_eq!(env.turn_duration_ms, 12400);
    }

    #[test]
    fn test_envelope_files_in_turn() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);
        let files = vec!["src/auth.rs".to_string(), "src/auth_test.rs".to_string()];

        let env = build_turn_envelope(&options, &files);
        assert_eq!(env.files_in_turn.len(), 2);
        assert!(env.files_in_turn.contains(&"src/auth.rs".to_string()));
        assert!(env.files_in_turn.contains(&"src/auth_test.rs".to_string()));
    }

    #[test]
    fn test_envelope_files_in_session() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let env = build_turn_envelope(&options, &[]);
        // Session already has 2 files touched
        assert_eq!(env.files_in_session, 2);
    }

    #[test]
    fn test_envelope_prompt_summary() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let env = build_turn_envelope(&options, &[]);
        assert_eq!(
            env.prompt_summary.as_deref(),
            Some("Fix the authentication bug in login.rs")
        );
    }

    #[test]
    fn test_envelope_prompt_hash() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let env = build_turn_envelope(&options, &[]);
        assert!(env.prompt_hash.is_some());
        let expected = blake3::hash(b"Fix the authentication bug in login.rs");
        assert_eq!(env.prompt_hash.unwrap(), *expected.as_bytes());
    }

    #[test]
    fn test_envelope_no_prompt() {
        let session = make_session();
        let event = make_event();
        let mut options = make_options(&session, &event);
        options.prompt = None;

        let env = build_turn_envelope(&options, &[]);
        assert_eq!(env.prompt_summary, None);
    }

    #[test]
    fn test_envelope_session_started_at() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);

        let env = build_turn_envelope(&options, &[]);
        assert_eq!(env.session_started_at, session.started_at.timestamp());
    }

    #[test]
    fn test_envelope_encodes_successfully() {
        let session = make_session();
        let event = make_event();
        let options = make_options(&session, &event);
        let files = vec!["src/auth.rs".to_string()];

        let env = build_turn_envelope(&options, &files);
        let bytes = env.encode().unwrap();
        assert!(SessionEnvelope::is_session_envelope(&bytes));

        // Roundtrip
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded.session_id, "sess-test-123");
        assert_eq!(decoded.turn_number, 3);
    }

    // record_turn tests (error cases — success requires a real repository)

    #[test]
    fn test_record_turn_nonexistent_repo_fails() {
        let session = make_session();
        let event = make_event();
        let options = TurnRecordOptions {
            session: &session,
            event: &event,
            turn_number: 3,
            turn_duration_ms: 5000,
            prompt: Some("Fix the bug".to_string()),
        };

        let result = record_turn(Path::new("/nonexistent/repo/path"), &options);
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentError::RecordFailed { reason, .. } => {
                assert!(
                    reason.contains("open repository") || reason.contains("Repository"),
                    "Unexpected reason: {}",
                    reason
                );
            }
            other => panic!("Expected RecordFailed, got: {:?}", other),
        }
    }

    // TurnRecordOutcome display

    #[test]
    fn test_outcome_display() {
        let outcome = TurnRecordOutcome {
            hash: Hash::of(b"test"),
            turn_number: 3,
            file_count: 2,
            message: "Turn 3: Fix the bug".to_string(),
            recorded_files: vec!["a.rs".to_string(), "b.rs".to_string()],
            unhashed_data: None,
        };

        let display = outcome.to_string();
        assert!(display.contains("Turn 3"));
        assert!(display.contains("2 files"));
    }

    #[test]
    fn test_outcome_display_singular() {
        let outcome = TurnRecordOutcome {
            hash: Hash::of(b"test"),
            turn_number: 1,
            file_count: 1,
            message: "Turn 1: Init".to_string(),
            recorded_files: vec!["a.rs".to_string()],
            unhashed_data: None,
        };

        let display = outcome.to_string();
        assert!(display.contains("1 file)"));
        assert!(!display.contains("1 files"));
    }

    #[test]
    fn test_outcome_recorded_file_list() {
        let outcome = TurnRecordOutcome {
            hash: Hash::of(b"test"),
            turn_number: 1,
            file_count: 2,
            message: "Turn 1".to_string(),
            recorded_files: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            unhashed_data: None,
        };

        assert_eq!(outcome.recorded_file_list(), &["src/main.rs", "src/lib.rs"]);
    }

    // should_ignore_untracked tests

    #[test]
    fn test_ignore_node_modules() {
        assert!(should_ignore_untracked("node_modules/express/index.js"));
        assert!(should_ignore_untracked("node_modules/.package-lock.json"));
    }

    #[test]
    fn test_ignore_target() {
        assert!(should_ignore_untracked("target/debug/atomic"));
        assert!(should_ignore_untracked("target/release/build/something"));
    }

    #[test]
    fn test_ignore_git() {
        assert!(should_ignore_untracked(".git/objects/pack/something"));
        assert!(should_ignore_untracked(".git/HEAD"));
    }

    #[test]
    fn test_ignore_claude_dir() {
        assert!(should_ignore_untracked(".claude/settings.json"));
    }

    #[test]
    fn test_ignore_pycache() {
        assert!(should_ignore_untracked(
            "__pycache__/module.cpython-311.pyc"
        ));
        assert!(should_ignore_untracked("src/__pycache__/something.pyc"));
    }

    #[test]
    fn test_ignore_hidden_dirs() {
        assert!(should_ignore_untracked(".vscode/settings.json"));
        assert!(should_ignore_untracked(".idea/workspace.xml"));
        assert!(should_ignore_untracked(".next/cache/webpack"));
    }

    #[test]
    fn test_ignore_nested_node_modules() {
        assert!(should_ignore_untracked(
            "packages/app/node_modules/lodash/index.js"
        ));
    }

    #[test]
    fn test_allow_normal_files() {
        assert!(!should_ignore_untracked("src/main.rs"));
        assert!(!should_ignore_untracked("README.md"));
        assert!(!should_ignore_untracked("src/auth/login.rs"));
        assert!(!should_ignore_untracked("tests/integration_test.rs"));
        assert!(!should_ignore_untracked("package.json"));
        assert!(!should_ignore_untracked("Cargo.toml"));
    }

    #[test]
    fn test_allow_dotfiles_in_root() {
        // Single-component hidden files (not dirs) at the root — these are
        // still filtered because they start with '.' and have len > 1.
        // This is intentional: .env, .gitignore, etc. are usually in
        // .atomicignore if the user wants them tracked.
        assert!(should_ignore_untracked(".env"));
        assert!(should_ignore_untracked(".gitignore"));
    }

    #[test]
    fn test_allow_files_with_dots_in_name() {
        // Files with dots in their name (not as a path component prefix)
        assert!(!should_ignore_untracked("src/config.production.ts"));
        assert!(!should_ignore_untracked("tsconfig.json"));
        assert!(!should_ignore_untracked("package-lock.json"));
    }

    #[test]
    fn test_outcome_recorded_file_list_empty() {
        let outcome = TurnRecordOutcome {
            hash: Hash::of(b"test"),
            turn_number: 1,
            file_count: 0,
            message: "Turn 1".to_string(),
            recorded_files: vec![],
            unhashed_data: None,
        };

        assert!(outcome.recorded_file_list().is_empty());
    }
}
