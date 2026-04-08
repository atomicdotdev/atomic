//! Change message construction for agent turns.
//!
//! Builds human-readable commit messages from turn context using a priority
//! system: meaningful prompt > file change summary > transcript summary > fallback.

use atomic_repository::status::{FileStatus, RepositoryStatus};

use super::options::TurnRecordOptions;
use crate::transcript;
use crate::transcript::CondensedEntry;

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
pub(crate) fn build_turn_message(
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
pub(crate) fn summarize_from_transcript(transcript_path: &std::path::Path) -> Option<String> {
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
    let last_tool_idx = entries.iter().rposition(|e: &CondensedEntry| e.is_tool())?; // If no tool calls, no useful summary

    // Find the first assistant text entry AFTER the last tool call.
    // Claude's wrap-up message typically comes right after the final tool use.
    let wrap_up = entries[last_tool_idx + 1..]
        .iter()
        .find(|e: &&CondensedEntry| e.is_assistant() && e.content.is_some())?;

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
pub(crate) fn extract_first_sentence(text: &str) -> String {
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
pub(crate) fn is_abbreviation(text: &str, dot_pos: usize) -> bool {
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
pub(crate) fn extract_first_sentence_from_paragraph(text: &str) -> String {
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
pub(crate) fn is_meaningful_prompt(prompt: &str) -> bool {
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
pub(crate) fn build_file_change_summary(
    status: &RepositoryStatus,
    untracked_paths: &[String],
) -> String {
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
pub(crate) fn format_file_group(verb: &str, files: &[String]) -> String {
    match files.len() {
        0 => String::new(),
        1 => format!("{} {}", verb, files[0]),
        2 => format!("{} {}, {}", verb, files[0], files[1]),
        3 => format!("{} {}, {}, {}", verb, files[0], files[1], files[2]),
        n => format!("{} {}, {} (+{} more)", verb, files[0], files[1], n - 2),
    }
}

/// Truncate a prompt to the given maximum length, adding "..." if needed.
pub(crate) fn truncate_prompt(prompt: &str, max_len: usize) -> String {
    let trimmed = prompt.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(max_len.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}
