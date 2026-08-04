use std::collections::HashMap;

use super::types::{
    AssistantMessage, CondensedEntry, EntryType, ToolInput, ToolUseSummary, TranscriptLine,
};

// Transcript Parsing (Claude Code JSONL)

/// Tools that should show only minimal detail (path/URL, not content).
const MINIMAL_DETAIL_TOOLS: &[&str] = &["Read", "Skill", "WebFetch"];

/// Prefix that identifies skill content injections in user messages.
///
/// Claude Code injects skill instructions as user messages after Skill tool
/// calls. These are verbose documentation, not user intent.
const SKILL_CONTENT_PREFIX: &str = "Base directory for this skill:";

/// Parse a Claude Code JSONL transcript into condensed entries.
///
/// Reads each line as JSON, extracts user prompts, assistant text responses,
/// and tool calls. Filters out:
/// - Skill content injections (verbose skill instructions in user messages)
/// - Full file contents from Read tool responses
/// - Verbose tool outputs
///
/// # Arguments
///
/// * `raw` — Raw JSONL bytes from the Claude Code transcript file
///
/// # Returns
///
/// A vector of condensed entries suitable for display and summarization.
/// Returns an empty vector if the transcript is empty or unparseable.
pub fn condense_claude_transcript(raw: &[u8]) -> Vec<CondensedEntry> {
    let mut entries = Vec::new();

    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }

        let Ok(parsed) = serde_json::from_slice::<TranscriptLine>(line) else {
            continue;
        };

        match parsed.r#type.as_str() {
            "user" => {
                if let Some(content) = extract_user_content(&parsed.message) {
                    // Skip skill content injections
                    if !content.starts_with(SKILL_CONTENT_PREFIX) {
                        entries.push(CondensedEntry::user(content));
                    }
                }
            }
            "assistant" => {
                if let Ok(msg) = serde_json::from_value::<AssistantMessage>(parsed.message) {
                    for block in &msg.content {
                        match block.r#type.as_str() {
                            "text" if !block.text.is_empty() => {
                                entries.push(CondensedEntry::assistant(&block.text));
                            }
                            "tool_use" => {
                                let detail = extract_tool_detail(&block.name, &block.input);
                                entries.push(CondensedEntry::tool(&block.name, detail));
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    entries
}

/// Condense a transcript from raw bytes, auto-detecting format.
///
/// Currently supports Claude Code JSONL. Future: Gemini JSON, other formats.
pub fn condense_transcript(raw: &[u8], format: &str) -> Vec<CondensedEntry> {
    match format {
        "jsonl" => condense_claude_transcript(raw),
        "opencode" => condense_opencode_transcript(raw),
        // Future: "json" => condense_gemini_transcript(raw),
        _ => {
            log::warn!("Unknown transcript format '{}', returning empty", format);
            Vec::new()
        }
    }
}

/// Parse a synthesized OpenCode JSONL transcript into condensed entries.
///
/// The line shape is produced by [`crate::transcript::opencode`] from
/// OpenCode's SQLite store: `user`/`assistant` lines carry `text`, `tool`
/// lines carry the tool name and an optional title. `reasoning` lines are
/// skipped — reasoning is carried separately by the change provenance and
/// the session graph's Decision nodes.
pub fn condense_opencode_transcript(raw: &[u8]) -> Vec<CondensedEntry> {
    let mut entries = Vec::new();

    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };

        match parsed.get("type").and_then(|v| v.as_str()) {
            Some("user") | Some("assistant") => {
                let Some(text) = parsed.get("text").and_then(|v| v.as_str()) else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                if parsed["type"] == "user" {
                    entries.push(CondensedEntry::user(text));
                } else {
                    entries.push(CondensedEntry::assistant(text));
                }
            }
            Some("tool") => {
                let name = parsed
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool");
                let title = parsed
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                entries.push(CondensedEntry::tool(name, title));
            }
            _ => {}
        }
    }

    entries
}

/// Extract the agent's final response text from a transcript.
///
/// Condenses the transcript and returns the text of the last assistant
/// entry — what the agent said last. This is the fallback source for the
/// session graph's `llm_response` node when the agent's stop payload does
/// not carry the response itself.
///
/// Returns `None` when the transcript has no assistant text.
pub fn last_assistant_text(raw: &[u8], format: &str) -> Option<String> {
    condense_transcript(raw, format)
        .into_iter()
        .rev()
        .find(|e| matches!(e.entry_type, EntryType::Assistant))
        .and_then(|e| e.content)
        .filter(|t| !t.trim().is_empty())
}

/// The transcript format produced by an agent's transcript file.
///
/// Used both when condensing a transcript for `agent_turn` data and when
/// deriving the agent's final response from it.
pub fn format_for_agent(agent_name: &str) -> &'static str {
    if agent_name.contains("gemini") {
        "json"
    } else if agent_name == "opencode" {
        "opencode"
    } else {
        "jsonl"
    }
}

/// Format condensed entries as human-readable text.
///
/// Output format:
/// ```text
/// [User] Fix the auth bug in login.rs
/// [Assistant] I'll fix the token validation...
/// [Tool] Edit: src/auth/login.rs
/// [Tool] Bash: cargo test
/// [Assistant] The fix is applied and tests pass.
///
/// [Files Modified]
/// - src/auth/login.rs
/// ```
///
/// This is the format passed to the reasoning generator and stored in
/// `UnhashedTurnData.condensed_text`.
pub fn format_condensed(entries: &[CondensedEntry], files: &[String]) -> String {
    let mut out = String::new();

    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&entry.to_string());
        out.push('\n');
    }

    if !files.is_empty() {
        out.push_str("\n[Files Modified]\n");
        for file in files {
            out.push_str("- ");
            out.push_str(file);
            out.push('\n');
        }
    }

    out
}

/// Extract user prompts from condensed entries.
pub fn extract_prompts(entries: &[CondensedEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.is_user())
        .filter_map(|e| e.content.clone())
        .collect()
}

/// Aggregate tool usage from condensed entries.
pub fn aggregate_tool_usage(entries: &[CondensedEntry]) -> Vec<ToolUseSummary> {
    let mut tools: HashMap<String, ToolUseSummary> = HashMap::new();

    for entry in entries.iter().filter(|e| e.is_tool()) {
        let name = entry.tool_name.as_deref().unwrap_or("Unknown");

        let summary = tools
            .entry(name.to_string())
            .or_insert_with(|| ToolUseSummary::new(name, 0, Vec::new()));

        summary.invocation_count += 1;

        // Track files affected by file-modifying tools
        if let Some(detail) = &entry.tool_detail {
            if matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
                && !summary.files_affected.contains(detail)
            {
                summary.files_affected.push(detail.clone());
            }
        }
    }

    let mut result: Vec<_> = tools.into_values().collect();
    result.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
    result
}

/// Extract user content from a transcript message value.
fn extract_user_content(message: &serde_json::Value) -> Option<String> {
    // User content can be a string or an array of content blocks
    if let Some(s) = message.get("content").and_then(|c| c.as_str()) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(arr) = message.get("content").and_then(|c| c.as_array()) {
        for item in arr {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
            // Also handle plain string items in the array
            if let Some(text) = item.as_str() {
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
        }
    }
    // Direct string message
    if let Some(s) = message.as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    None
}

/// Extract appropriate detail for a tool call.
///
/// For minimal-detail tools (Read, Skill, WebFetch), returns only the
/// essential identifier (path, skill name, URL). For other tools, returns
/// the most relevant field from the input.
fn extract_tool_detail(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    let parsed: ToolInput = serde_json::from_value(input.clone()).unwrap_or_default();

    // Minimal detail tools — show only the identifier
    if MINIMAL_DETAIL_TOOLS.contains(&tool_name) {
        return match tool_name {
            "Skill" => parsed.skill,
            "Read" => parsed.file_path.or(parsed.notebook_path),
            "WebFetch" => parsed.url,
            _ => None,
        };
    }

    // Other tools — use the most relevant field
    parsed
        .description
        .or(parsed.command)
        .or(parsed.file_path)
        .or(parsed.notebook_path)
        .or(parsed.pattern)
}
