//! Provenance, envelope, and unhashed data construction for agent turns.
//!
//! Contains the AI provenance builder, session envelope construction,
//! transcript-based unhashed data assembly, and ignore patterns for
//! filtering untracked files during auto-add.

use atomic_core::change::{
    AITool, AIVendor, Cost, PromptContent, Provenance, SuggestionType, TokenUsage,
};
use atomic_core::types::Hash;

use crate::envelope::SessionEnvelope;
use crate::transcript;

use super::message::is_meaningful_prompt;
use super::message::truncate_prompt;
use super::options::TurnRecordOptions;

// Provenance Construction

/// Build a `Provenance` entry for an agent turn.
///
/// Populates vendor, model, tool, suggestion type, session ID, and prompt hash.
/// Token usage and cost can be added later when that data is available from
/// the agent's transcript.
pub(crate) fn build_turn_provenance(options: &TurnRecordOptions<'_>) -> Provenance {
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
pub(crate) fn vendor_from_agent_name(agent_name: &str) -> AIVendor {
    match agent_name {
        "claude-code" => AIVendor::Anthropic,
        "gemini-cli" => AIVendor::Google,
        "agy" => AIVendor::Google,
        "codex" => AIVendor::OpenAI,
        "kiro" => AIVendor::AmazonBedrock,
        "copilot" => AIVendor::Other("github".to_string()),
        "cursor" => AIVendor::Anthropic,
        "opencode" => AIVendor::OpenAI,
        _ => AIVendor::Other(agent_name.to_string()),
    }
}

// SessionEnvelope Construction

/// Build a `SessionEnvelope` for embedding in `HashedChange.metadata`.
pub(crate) fn build_turn_envelope(
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

// Step 6 Helper: Build Unhashed Turn Data

/// Build the unhashed turn data (transcript + reasoning) from the agent's
/// transcript file and the recorded change's FileOps.
///
/// Returns `None` if the transcript is not available, empty, or unparseable.
/// Reasoning generation failure is logged and skipped (non-fatal).
pub(crate) fn build_unhashed_turn_data(
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
pub(crate) const AUTO_ADD_IGNORE_DIRS: &[&str] = &[
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
/// Hidden directories that should NOT be ignored by the auto-add filter.
/// These are Atomic-managed directories that contain tracked content.
const HIDDEN_DIR_WHITELIST: &[&str] = &[".vault", ".atomicignore"];

pub(crate) fn should_ignore_untracked(path: &str) -> bool {
    for component in std::path::Path::new(path).components() {
        if let std::path::Component::Normal(name) = component {
            let name_str = name.to_string_lossy();
            if AUTO_ADD_IGNORE_DIRS.contains(&name_str.as_ref()) {
                return true;
            }
            // Skip hidden directories (starting with .) unless whitelisted
            if name_str.starts_with('.')
                && name_str.len() > 1
                && !HIDDEN_DIR_WHITELIST.contains(&name_str.as_ref())
            {
                return true;
            }
        }
    }
    false
}
