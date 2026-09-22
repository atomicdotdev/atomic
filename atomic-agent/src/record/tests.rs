//! Tests for the record module.

use super::message::*;
use super::options::*;
use super::provenance::*;
use super::*;

use std::path::Path;

use atomic_core::change::{AITool, AIVendor, PromptContent, SuggestionType};
use atomic_core::types::Hash;

use crate::envelope::SessionEnvelope;
use crate::event::{HookType, TurnEvent};
use crate::turn::session::AgentSession;

use atomic_repository::status::RepositoryStatus;

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
    let lines = [
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
    let text = "Created src/index.ts, package.json, and tsconfig.json for the project. Tests pass.";
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
    let lines = [
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
    let lines = [
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
    let lines = [
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

    let lines = [
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
    let lines = [
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
fn test_vendor_from_agent_name_grok() {
    assert_eq!(vendor_from_agent_name("grok"), AIVendor::XAI);
}

#[test]
fn test_vendor_from_agent_name_kiro() {
    assert_eq!(vendor_from_agent_name("kiro"), AIVendor::AmazonBedrock);
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
        git_transition: None,
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
        git_transition: None,
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
        git_transition: None,
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
        git_transition: None,
    };

    assert!(outcome.recorded_file_list().is_empty());
}

// Orphaned session view duplication

/// Build a Go-like source file with `count` distinct top-level functions,
/// each with a unique, greppable marker line.
fn build_source(count: usize) -> String {
    let mut out = String::new();
    out.push_str("package main\n\n");
    for i in 0..count {
        out.push_str(&format!(
            "func step{i}() int {{\n    // marker[{i}]\n    return {i}\n}}\n\n"
        ));
    }
    out
}

fn count_occurrences(content: &str, pattern: &str) -> usize {
    content.matches(pattern).count()
}

/// Reproduces the real-world mechanism behind the orphan-view duplication
/// bug: a session whose `SessionStart` fork never ran (or failed silently),
/// so its view doesn't exist yet when `record_turn()` is called for the
/// first time.
///
/// Before the fix, `record_turn()` would fall through to whatever view
/// happened to be `current_view` on the raw `Repository` handle it opens
/// internally — not necessarily this session's intended parent — and record
/// the turn there. Once that view and the session's own (separately
/// forked) view were both later merged into `dev`, the divergent baselines
/// meant the same pre-existing content could be recorded twice.
///
/// This test drives two sessions through `record_turn()` directly: session
/// A whose view is properly forked from `dev` beforehand (the normal path),
/// and session B whose view is deliberately left unforked (the orphan
/// path). Per the fix, `record_turn()` must self-heal session B by forking
/// its view just-in-time from its intended parent (`dev`) rather than
/// silently recording onto the wrong view. After merging both sessions'
/// views into `dev`, every function in the file must appear exactly once.
#[test]
fn test_orphaned_session_view_duplicates_content_on_merge() {
    use atomic_repository::apply::CrossViewInsertOptions;
    use atomic_repository::Repository;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let repo_root = temp_dir.path();
    let file = repo_root.join("main.go");

    let initial = build_source(80);
    std::fs::write(&file, &initial).unwrap();

    {
        let repo = Repository::init(repo_root).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.add(
            working_copy,
            "main.go",
            atomic_repository::tracking::TrackingOptions::default(),
        )
        .unwrap();
        let header = atomic_core::change::ChangeHeader::new("Add main.go");
        let options = atomic_repository::record::RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(working_copy, header, options).unwrap();
    }

    // Session A: SessionStart properly forks its view from "dev" before any
    // turn is recorded — the normal, non-buggy path.
    {
        let mut repo = Repository::open_existing(repo_root).unwrap();
        repo.create_view_from("session-a", "dev").unwrap();
    }

    let mut session_a = AgentSession::new("session-a", "claude-code", "Claude Code");
    session_a.view_name = "session-a".to_string();
    session_a.set_parent_view("dev");

    let edited_a = initial.replacen("return 10\n", "return 1000\n", 1);
    std::fs::write(&file, &edited_a).unwrap();

    let event_a = TurnEvent::new("session-a", HookType::TurnEnd);
    let options_a = TurnRecordOptions {
        session: &session_a,
        event: &event_a,
        turn_number: 1,
        turn_duration_ms: 1000,
        prompt: Some("Bump step10".to_string()),
    };
    record_turn(repo_root, &options_a).unwrap();

    // Session B: simulates a SessionStart fork that never ran (or failed) —
    // its view does not exist yet when record_turn() is called. Per the
    // orphan-view duplication fix, record_turn() must self-heal by forking
    // it just-in-time from its intended parent ("dev"), NOT from whatever
    // view happens to be current on the internally-opened Repository handle
    // (which after session A's turn is "session-a" — forking from there
    // would leak session A's edit into session B's history and, once both
    // are merged into dev, resurrect it as a duplicate).
    let mut session_b = AgentSession::new("session-b", "claude-code", "Claude Code");
    session_b.view_name = "session-b".to_string();
    session_b.set_parent_view("dev");

    let edited_b = initial.replacen("return 70\n", "return 7000\n", 1);
    std::fs::write(&file, &edited_b).unwrap();

    let event_b = TurnEvent::new("session-b", HookType::TurnEnd);
    let options_b = TurnRecordOptions {
        session: &session_b,
        event: &event_b,
        turn_number: 1,
        turn_duration_ms: 1000,
        prompt: Some("Bump step70".to_string()),
    };
    record_turn(repo_root, &options_b)
        .expect("record_turn should self-heal an orphaned session view rather than fail");

    // Merge both session views into a merge target.
    //
    // CB-12B: insertion into a Shared view is a gated publication boundary
    // now, so this merge-semantics regression exercises a Draft fork of dev
    // instead — identical graph semantics (same parent chain, same change
    // closure), no publication policy in play. The no-duplicate assertion
    // below is unchanged.
    {
        let mut repo = Repository::open_existing(repo_root).unwrap();
        repo.create_view_from("dev-merge", "dev").unwrap();
        repo.insert_from_view(CrossViewInsertOptions::new("session-a", "dev-merge"))
            .unwrap();
        repo.insert_from_view(CrossViewInsertOptions::new("session-b", "dev-merge"))
            .unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.materialize(working_copy).unwrap();
    }

    let repo = Repository::open_existing(repo_root).unwrap();
    let content = repo
        .get_file_content_on_view("main.go", "dev-merge")
        .unwrap()
        .expect("file should exist on the merge target");
    let content = String::from_utf8(content).unwrap();

    for i in 0..80 {
        let marker = format!("// marker[{i}]");
        let occurrences = count_occurrences(&content, &marker);
        assert_eq!(
            occurrences, 1,
            "step{} should appear exactly once after merging both session views, \
             found {} (orphan-view duplication bug)",
            i, occurrences
        );
    }

    assert!(
        content.contains("return 1000"),
        "session A's edit should survive the merge"
    );
    assert!(
        content.contains("return 7000"),
        "session B's edit should survive the merge"
    );
}

// CB-12A: git-only turn classification (RFC §10.2). A clean worktree is
// classified — never reported as an empty turn.
mod cb12a_classification {
    use super::*;
    use crate::turn::capture;
    use crate::turn::session::SessionStore;
    use atomic_core::change::session::{ManagedTurnOutcome, SessionIncompleteOrigin};
    use atomic_repository::tracking::TrackingOptions;
    use atomic_repository::Repository;
    use std::path::Path;
    use tempfile::TempDir;

    fn staged_git_token(root: &Path) -> atomic_repository::GitObservationToken {
        atomic_repository::observe_git_metadata(root)
            .unwrap()
            .token()
    }

    fn commit_file(git: &git2::Repository, root: &Path, name: &str, content: &str) -> String {
        std::fs::write(root.join(name), content).unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new(name)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Turn Test", "turn@test.invalid").unwrap();
        let parent = git.head().ok().and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        git.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "test commit",
            &tree,
            &parents,
        )
        .unwrap()
        .to_string()
    }

    fn make_options<'a>(
        session: &'a AgentSession,
        event: &'a TurnEvent,
        turn: u32,
    ) -> TurnRecordOptions<'a> {
        TurnRecordOptions {
            session,
            event,
            turn_number: turn,
            turn_duration_ms: 1,
            prompt: Some("git-only turn".to_string()),
        }
    }

    /// Colocated atomic+git repo with `file` tracked in Atomic (baseline) and
    /// a turn-start boundary captured. The session is saved to disk like the
    /// orchestrator does at turn start.
    fn setup(
        dir: &TempDir,
        session_id: &str,
        file: &str,
        content: &str,
    ) -> (git2::Repository, AgentSession) {
        let git = git2::Repository::init(dir.path()).unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut session = AgentSession::new(session_id, "claude-code", "Claude Code");
        session.view_name = repo.current_view().to_string();

        // Baseline: Atomic tracks the same content Git will commit, so the
        // worktree stays clean across the git-only turn.
        std::fs::write(dir.path().join(file), content).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.add(working_copy, file, TrackingOptions::default())
            .unwrap();
        repo.record(
            working_copy,
            atomic_core::change::ChangeHeader::new("baseline"),
            atomic_repository::record::RecordOptions::new(),
        )
        .unwrap();

        let baseline = crate::record::capture_turn_boundary(&repo, dir.path(), session_id, 1)
            .expect("turn-start boundary captures on a real repository");
        session.set_boundary_start(baseline.clone());
        drop(repo);

        // The orchestrator persisted the session (with its boundary) at turn
        // start; the pre-commit hook loads it from disk.
        let store = SessionStore::new(dir.path().join(".atomic").join("sessions")).unwrap();
        store.save(&session).unwrap();
        (git, session)
    }

    #[test]
    fn clean_turn_without_git_move_is_observation_only() {
        let dir = TempDir::new().unwrap();
        let (_git, mut session) = setup(&dir, "sess-obs-only", "tracked.txt", "same\n");
        let event = TurnEvent::new("sess-obs-only", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Classified(classified) => {
                assert_eq!(classified.outcome, ManagedTurnOutcome::ObservationOnly);
                assert!(classified.incomplete.is_none());
                assert!(classified.boundary_end.is_some());
            }
            TurnRecordResult::Recorded(_) => {
                panic!("a clean turn with no git move must classify, not record")
            }
        }
        let _ = &mut session;
    }

    #[test]
    fn clean_turn_with_unexplained_commit_is_incomplete() {
        let dir = TempDir::new().unwrap();
        let (git, session) = setup(&dir, "sess-unexplained", "tracked.txt", "same\n");

        // A commit happens inside the turn window without any pre-commit
        // capture (hook bypassed / --no-verify).
        let new_head = commit_file(&git, dir.path(), "tracked.txt", "same\n");

        let event = TurnEvent::new("sess-unexplained", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Classified(classified) => {
                match &classified.outcome {
                    ManagedTurnOutcome::RepositoryOperations {
                        operations,
                        capture,
                    } => {
                        assert!(capture.is_none());
                        assert!(
                            operations
                                .iter()
                                .any(|operation| operation.contains("HEAD")),
                            "the observed HEAD transition must be described: {operations:?}"
                        );
                    }
                    other => panic!("expected RepositoryOperations, got {other:?}"),
                }
                let incomplete = classified
                    .incomplete
                    .expect("unexplained commit refuses attribution");
                assert_eq!(
                    incomplete.origin,
                    SessionIncompleteOrigin::UnattributedGitOperation
                );
                assert_eq!(
                    incomplete.unbound_commits,
                    vec![new_head],
                    "the observed commit is durably unbound evidence"
                );
            }
            TurnRecordResult::Recorded(_) => panic!("a git-only turn must classify, not record"),
        }
    }

    #[test]
    fn clean_turn_with_verified_capture_binds_but_never_claims_managed_capture() {
        let dir = TempDir::new().unwrap();
        let (git, session) = setup(&dir, "sess-captured", "tracked.txt", "same\n");
        let sessions_dir = dir.path().join(".atomic").join("sessions");

        // The pre-commit hook path: stage the exact tree, capture (HEAD =
        // parent, index tree = the tree about to be committed), sign,
        // persist, then the commit lands.
        let mut index = git.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        drop(index);
        let commit_time_token = staged_git_token(dir.path());
        {
            let store = SessionStore::new(&sessions_dir).unwrap();
            let mut stored = store.load("sess-captured").unwrap().unwrap();
            let working_copy = stored.boundary_start.clone().unwrap().working_copy;
            capture::write_capture(
                &sessions_dir,
                &mut stored,
                1,
                working_copy,
                &commit_time_token,
            )
            .unwrap();
            store.save(&stored).unwrap();
        }
        {
            let mut index = git.index().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = git.find_tree(tree_id).unwrap();
            let signature = git2::Signature::now("Turn Test", "turn@test.invalid").unwrap();
            let parent = git.head().ok().and_then(|head| head.peel_to_commit().ok());
            let parents: Vec<&git2::Commit> = parent.iter().collect();
            git.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "captured commit",
                &tree,
                &parents,
            )
            .unwrap();
        }

        let event = TurnEvent::new("sess-captured", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Classified(classified) => {
                // RFC §19 Q2 APPROVED (allow-as-incomplete, 2026-09-14): a
                // verified capture binds the transition but, without the
                // exact §10.3.2 reassembly, the session stays DURABLY
                // INCOMPLETE with observed-operation-only attribution.
                let incomplete = classified.incomplete.as_ref().expect(
                    "a verified capture without the exact reassembly must mark the session durably incomplete",
                );
                assert_eq!(
                    incomplete.origin,
                    SessionIncompleteOrigin::ManagedCaptureAwaitingReassembly,
                    "the durable origin names the awaiting-reassembly policy: {incomplete:?}"
                );
                assert!(
                    !incomplete.unbound_commits.is_empty(),
                    "the observed commit is retained as an unbound commit: {incomplete:?}"
                );
                match &classified.outcome {
                    ManagedTurnOutcome::RepositoryOperations { capture, .. } => {
                        assert!(
                            capture.is_some(),
                            "the verified capture hash must bind the transition"
                        );
                    }
                    other => panic!("expected RepositoryOperations, got {other:?}"),
                }
            }
            TurnRecordResult::Recorded(_) => panic!("a git-only turn must classify, not record"),
        }
    }

    #[test]
    fn clean_turn_with_tampered_capture_is_incomplete() {
        let dir = TempDir::new().unwrap();
        let (git, session) = setup(&dir, "sess-tampered", "tracked.txt", "same\n");
        let sessions_dir = dir.path().join(".atomic").join("sessions");

        // Capture, then tamper with a covered field before the commit lands.
        let mut index = git.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        drop(index);
        let token = staged_git_token(dir.path());
        {
            let store = SessionStore::new(&sessions_dir).unwrap();
            let mut stored = store.load("sess-tampered").unwrap().unwrap();
            let working_copy = stored.boundary_start.clone().unwrap().working_copy;
            capture::write_capture(&sessions_dir, &mut stored, 1, working_copy, &token).unwrap();
            store.save(&stored).unwrap();

            let path = {
                let files = std::fs::read_dir(
                    capture::capture_dir(&sessions_dir, "sess-tampered").unwrap(),
                )
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .collect::<Vec<_>>();
                assert_eq!(files.len(), 1, "exactly one capture attempt exists");
                files.into_iter().next().unwrap()
            };
            let mut written =
                capture::ManagedCommitCapture::from_json(&std::fs::read(&path).unwrap()).unwrap();
            written.index_tree = Some("forged-tree".to_string());
            std::fs::write(&path, written.to_json().unwrap()).unwrap();
        }
        {
            let mut index = git.index().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = git.find_tree(tree_id).unwrap();
            let signature = git2::Signature::now("Turn Test", "turn@test.invalid").unwrap();
            let parent = git.head().ok().and_then(|head| head.peel_to_commit().ok());
            let parents: Vec<&git2::Commit> = parent.iter().collect();
            git.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "tampered capture",
                &tree,
                &parents,
            )
            .unwrap();
        }

        let event = TurnEvent::new("sess-tampered", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Classified(classified) => {
                let incomplete = classified
                    .incomplete
                    .expect("a tampered capture must refuse attribution");
                assert_eq!(
                    incomplete.origin,
                    SessionIncompleteOrigin::UnattributedGitOperation
                );
                match &classified.outcome {
                    ManagedTurnOutcome::RepositoryOperations { capture, .. } => {
                        assert!(capture.is_none());
                    }
                    other => panic!("expected RepositoryOperations, got {other:?}"),
                }
            }
            TurnRecordResult::Recorded(_) => panic!("a git-only turn must classify, not record"),
        }
    }

    #[test]
    fn clean_turn_with_journaled_checkout_is_explained() {
        let dir = TempDir::new().unwrap();
        let git = git2::Repository::init(dir.path()).unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut session = AgentSession::new("sess-checkout", "claude-code", "Claude Code");
        session.view_name = repo.current_view().to_string();

        // Baseline + first commit so HEAD is real.
        std::fs::write(dir.path().join("tracked.txt"), "same\n").unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.add(working_copy, "tracked.txt", TrackingOptions::default())
            .unwrap();
        repo.record(
            working_copy,
            atomic_core::change::ChangeHeader::new("baseline"),
            atomic_repository::record::RecordOptions::new(),
        )
        .unwrap();
        commit_file(&git, dir.path(), "tracked.txt", "same\n");

        // Boundary AFTER the first commit: the turn window starts at C1.
        let baseline =
            crate::record::capture_turn_boundary(&repo, dir.path(), "sess-checkout", 1).unwrap();
        let start_oid = baseline.git.as_ref().unwrap().head_oid.clone().unwrap();
        session.set_boundary_start(baseline);
        drop(repo);
        SessionStore::new(dir.path().join(".atomic").join("sessions"))
            .unwrap()
            .save(&session)
            .unwrap();

        // A second commit with identical content moves HEAD (C1 -> C2) with
        // no worktree change; the journal explains it as a checkout.
        let end_oid = commit_file(&git, dir.path(), "tracked.txt", "same\n");
        assert_ne!(start_oid, end_oid);

        // A genuine-shape journal row: real worktree root, in-window time.
        // Built through serde_json so windows backslash paths are escaped
        // correctly (a raw format! emitted invalid JSON escapes on windows
        // and the reader dropped the row as unparsable).
        let journal = dir.path().join(".atomic").join("bridge");
        std::fs::create_dir_all(&journal).unwrap();
        let recorded_at = chrono::Utc::now().to_rfc3339();
        let record = serde_json::json!({
            "version": 1,
            "record_type": "post-checkout",
            "event_id": "e1",
            "recorded_at": recorded_at,
            "advisory": true,
            "old_head": start_oid,
            "new_head": end_oid,
            "checkout_kind": "branch",
            "worktree_root": dir.path().display().to_string(),
        })
        .to_string();
        std::fs::write(journal.join("git-events.jsonl"), format!("{record}\n")).unwrap();

        let event = TurnEvent::new("sess-checkout", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Classified(classified) => {
                assert!(
                    classified.incomplete.is_none(),
                    "a genuine in-window checkout row explains the move: {:?}",
                    classified.incomplete
                );
                match &classified.outcome {
                    ManagedTurnOutcome::RepositoryOperations { capture, .. } => {
                        assert!(capture.is_none());
                    }
                    other => panic!("expected RepositoryOperations, got {other:?}"),
                }
            }
            TurnRecordResult::Recorded(_) => panic!("a git-only turn must classify, not record"),
        }
    }

    /// Review ATOM::aaron::8 R4 (executed probe): a hand-written stale
    /// advisory row with a wrong worktree root must NOT clear incompleteness.
    /// Advisory journal evidence is anchored to the worktree and the turn
    /// window before it can explain a transition.
    #[test]
    fn forged_stale_checkout_row_does_not_clear_incomplete() {
        let dir = TempDir::new().unwrap();
        let git = git2::Repository::init(dir.path()).unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut session = AgentSession::new("sess-forged", "claude-code", "Claude Code");
        session.view_name = repo.current_view().to_string();

        std::fs::write(dir.path().join("tracked.txt"), "same\n").unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.add(working_copy, "tracked.txt", TrackingOptions::default())
            .unwrap();
        repo.record(
            working_copy,
            atomic_core::change::ChangeHeader::new("baseline"),
            atomic_repository::record::RecordOptions::new(),
        )
        .unwrap();
        commit_file(&git, dir.path(), "tracked.txt", "same\n");

        let baseline =
            crate::record::capture_turn_boundary(&repo, dir.path(), "sess-forged", 1).unwrap();
        let start_oid = baseline.git.as_ref().unwrap().head_oid.clone().unwrap();
        session.set_boundary_start(baseline);
        drop(repo);
        SessionStore::new(dir.path().join(".atomic").join("sessions"))
            .unwrap()
            .save(&session)
            .unwrap();

        let end_oid = commit_file(&git, dir.path(), "tracked.txt", "same\n");
        assert_ne!(start_oid, end_oid);

        // The exact forged row from the review's probe: stale timestamp and
        // a worktree root of "/" that cannot be this worktree.
        let journal = dir.path().join(".atomic").join("bridge");
        std::fs::create_dir_all(&journal).unwrap();
        let record = format!(
            "{{\"version\":1,\"record_type\":\"post-checkout\",\"event_id\":\"e1\",\
             \"recorded_at\":\"2026-09-13T00:00:00+00:00\",\"advisory\":true,\
             \"old_head\":\"{start_oid}\",\"new_head\":\"{end_oid}\",\
             \"checkout_kind\":\"branch\",\"worktree_root\":\"/\"}}\n"
        );
        std::fs::write(journal.join("git-events.jsonl"), record).unwrap();

        let event = TurnEvent::new("sess-forged", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Classified(classified) => {
                let incomplete = classified
                    .incomplete
                    .expect("a stale, wrong-worktree advisory row must not clear incompleteness");
                assert_eq!(
                    incomplete.origin,
                    SessionIncompleteOrigin::UnattributedGitOperation
                );
                assert_eq!(incomplete.unbound_commits, vec![end_oid]);
            }
            TurnRecordResult::Recorded(_) => panic!("a git-only turn must classify, not record"),
        }
    }

    /// Review ATOM::aaron::8 R2 (executed probe): a mixed turn — working-copy
    /// edit AND a Git commit with no capture — records its content AND
    /// carries a durable refusal for the unexplained transition. Dirty
    /// content alone never bypasses capture classification.
    #[test]
    fn dirty_turn_with_unexplained_commit_marks_incomplete() {
        let dir = TempDir::new().unwrap();
        let (git, session) = setup(&dir, "sess-dirty-bypass", "tracked.txt", "baseline\n");

        // Mixed turn: uncovered edit + Git commit inside the same window.
        std::fs::write(dir.path().join("tracked.txt"), "uncovered edit\n").unwrap();
        let new_head = commit_file(&git, dir.path(), "tracked.txt", "uncovered edit\n");

        let event = TurnEvent::new("sess-dirty-bypass", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Recorded(outcome) => {
                let transition = outcome
                    .git_transition
                    .expect("the mixed turn must classify its Git transition");
                let incomplete = transition
                    .incomplete
                    .expect("an unexplained commit inside a dirty turn refuses attribution");
                assert_eq!(
                    incomplete.origin,
                    SessionIncompleteOrigin::UnattributedGitOperation
                );
                assert_eq!(incomplete.unbound_commits, vec![new_head]);
                assert!(
                    transition.operations.iter().any(|op| op.contains("HEAD")),
                    "the observed HEAD transition must be described: {:?}",
                    transition.operations
                );
            }
            TurnRecordResult::Classified(_) => {
                panic!("dirty content must record, with its transition classified alongside")
            }
        }
    }

    /// Review R2: a mixed turn whose commit carries a verified capture binds
    /// the transition WITHOUT a refusal, while the content records normally.
    #[test]
    fn dirty_turn_with_verified_capture_binds_transition() {
        let dir = TempDir::new().unwrap();
        let (git, session) = setup(&dir, "sess-dirty-captured", "tracked.txt", "same\n");
        let sessions_dir = dir.path().join(".atomic").join("sessions");

        // Capture at commit time (parent HEAD, index tree), then commit, then
        // leave additional dirty content for the recorded turn.
        let mut index = git.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        drop(index);
        let token = staged_git_token(dir.path());
        {
            let store = SessionStore::new(&sessions_dir).unwrap();
            let mut stored = store.load("sess-dirty-captured").unwrap().unwrap();
            let working_copy = stored.boundary_start.clone().unwrap().working_copy;
            capture::write_capture(&sessions_dir, &mut stored, 1, working_copy, &token).unwrap();
            store.save(&stored).unwrap();
        }
        {
            let mut index = git.index().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = git.find_tree(tree_id).unwrap();
            let signature = git2::Signature::now("Turn Test", "turn@test.invalid").unwrap();
            let parent = git.head().ok().and_then(|head| head.peel_to_commit().ok());
            let parents: Vec<&git2::Commit> = parent.iter().collect();
            git.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "captured",
                &tree,
                &parents,
            )
            .unwrap();
        }
        // Additional content work after the commit, inside the same turn.
        std::fs::write(dir.path().join("extra.txt"), "later work\n").unwrap();

        let event = TurnEvent::new("sess-dirty-captured", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Recorded(outcome) => {
                let transition = outcome
                    .git_transition
                    .expect("the mixed turn must classify its Git transition");
                // RFC §19 Q2 APPROVED (allow-as-incomplete): the capture
                // binds the transition while the session stays durably
                // incomplete until the exact §10.3.2 reassembly attributes
                // the commit.
                let incomplete = transition.incomplete.as_ref().expect(
                    "a verified capture without the exact reassembly must mark the turn durably incomplete",
                );
                assert_eq!(
                    incomplete.origin,
                    SessionIncompleteOrigin::ManagedCaptureAwaitingReassembly
                );
                assert!(transition.capture.is_some());
            }
            TurnRecordResult::Classified(_) => {
                panic!("dirty content must record, with its transition classified alongside")
            }
        }
    }

    /// CB-12A follow-up AC-8 (exact separable case): a dirty turn whose
    /// worktree carries NO remainder beyond the verified commit (everything
    /// staged and committed, nothing untracked) records the commit delta
    /// EXACTLY — ManagedGitCommitCaptured: incomplete is None and the
    /// exact_commit_oid names the commit. The Atomic change and the Git
    /// commit carry the same content.
    #[test]
    fn dirty_turn_with_verified_capture_and_no_remainder_is_exact() {
        let dir = TempDir::new().unwrap();
        let (git, session) = setup(&dir, "sess-exact", "tracked.txt", "same\n");
        let sessions_dir = dir.path().join(".atomic").join("sessions");

        // Change the tracked content, stage it, capture at commit time,
        // then commit — with NO content left behind (the worktree equals
        // the commit tree, and the commit content differs from the view
        // baseline so the turn records the delta).
        std::fs::write(dir.path().join("tracked.txt"), "exact delta\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        drop(index);
        let token = staged_git_token(dir.path());
        {
            let store = SessionStore::new(&sessions_dir).unwrap();
            let mut stored = store.load("sess-exact").unwrap().unwrap();
            let working_copy = stored.boundary_start.clone().unwrap().working_copy;
            capture::write_capture(&sessions_dir, &mut stored, 1, working_copy, &token).unwrap();
            store.save(&stored).unwrap();
        }
        let commit_oid = {
            let mut index = git.index().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = git.find_tree(tree_id).unwrap();
            let signature = git2::Signature::now("Turn Test", "turn@test.invalid").unwrap();
            let parent = git.head().ok().and_then(|head| head.peel_to_commit().ok());
            let parents: Vec<&git2::Commit> = parent.iter().collect();
            let oid = git
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    "captured exact",
                    &tree,
                    &parents,
                )
                .unwrap();
            oid.to_string()
        };

        let event = TurnEvent::new("sess-exact", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Recorded(outcome) => {
                let transition = outcome
                    .git_transition
                    .expect("the mixed turn must classify its Git transition");
                assert!(
                    transition.incomplete.is_none(),
                    "the exact case reports complete coverage for the commit: {:?}",
                    transition.incomplete
                );
                assert_eq!(
                    transition.exact_commit_oid.as_deref(),
                    Some(commit_oid.as_str()),
                    "the exact binding names the commit"
                );
                assert!(transition.capture.is_some());
            }
            TurnRecordResult::Classified(_) => {
                panic!("the exact turn records content with its transition classified alongside")
            }
        }
    }

    /// Review ATOM::aaron::8 R3 (executed probe): a missing turn-start
    /// baseline is a durable refusal, not a silently successful
    /// RepositoryOperations classification.
    #[test]
    fn missing_turn_start_baseline_is_durable_incomplete() {
        let dir = TempDir::new().unwrap();
        let (_git, mut session) = setup(&dir, "sess-no-baseline", "tracked.txt", "same\n");
        session.clear_boundary_start();
        SessionStore::new(dir.path().join(".atomic").join("sessions"))
            .unwrap()
            .save(&session)
            .unwrap();

        let event = TurnEvent::new("sess-no-baseline", HookType::TurnEnd);
        let options = make_options(&session, &event, 1);

        match record_turn(dir.path(), &options).unwrap() {
            TurnRecordResult::Classified(classified) => {
                let incomplete = classified
                    .incomplete
                    .expect("a missing baseline must refuse attribution durably");
                assert_eq!(
                    incomplete.origin,
                    SessionIncompleteOrigin::ObservationUnavailable
                );
                match &classified.outcome {
                    ManagedTurnOutcome::RepositoryOperations { .. } => {}
                    other => panic!("expected RepositoryOperations, got {other:?}"),
                }
            }
            TurnRecordResult::Recorded(_) => panic!("a git-only turn must classify, not record"),
        }
    }
}

/// Native agent recording (`record_turn` — the same entry the Stop hook uses)
/// must invoke the name-conflict resolver for a *content-clean* conflict where
/// the working tree already holds exactly one incarnation's bytes, and it must
/// keep the session envelope. Disposable colocated, unanchored repo: no Atomic
/// Git checkpoint exists, mirroring the live Stop-hook conditions.
#[test]
fn record_turn_resolves_content_clean_name_conflict_with_provenance() {
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let repo_root = dir.path();
    let _git = git2::Repository::init(repo_root).unwrap();

    fn record_all(
        repo: &atomic_repository::Repository,
        wc: atomic_core::WorkingCopyId,
        message: &str,
    ) {
        let header = atomic_core::change::ChangeHeader::new(message);
        repo.record(
            wc,
            header,
            atomic_repository::record::RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    }
    fn add(repo: &atomic_repository::Repository, wc: atomic_core::WorkingCopyId, path: &str) {
        repo.add(
            wc,
            path,
            atomic_repository::tracking::TrackingOptions::default(),
        )
        .unwrap();
    }

    {
        let mut repo = atomic_repository::Repository::init(repo_root).unwrap();
        let wc = repo.require_working_copy_id().unwrap();

        std::fs::write(repo_root.join("seed.txt"), b"seed\n").unwrap();
        add(&repo, wc, "seed.txt");
        record_all(&repo, wc, "base");

        repo.create_view_from("feature", "dev").unwrap();

        repo.switch_view(wc, "feature").unwrap();
        std::fs::write(repo_root.join("new.txt"), b"from-feature\n").unwrap();
        add(&repo, wc, "new.txt");
        record_all(&repo, wc, "feature creates new.txt");

        repo.switch_view(wc, "dev").unwrap();
        std::fs::write(repo_root.join("new.txt"), b"from-base\n").unwrap();
        add(&repo, wc, "new.txt");
        record_all(&repo, wc, "dev creates new.txt");

        repo.insert_from_view(atomic_repository::CrossViewInsertOptions::new(
            "feature", "dev",
        ))
        .unwrap();

        // The user resolves the ambiguity on disk to exactly one side's bytes,
        // with no persisted conflict row and no markers.
        std::fs::write(repo_root.join("new.txt"), b"from-base\n").unwrap();
    }

    // A real turn-start boundary (the user-prompt hook captures this).
    let baseline = {
        let repo = atomic_repository::Repository::open_readonly(repo_root).unwrap();
        crate::record::capture_turn_boundary(&repo, repo_root, "sess-conflict", 1).unwrap()
    };

    let mut session = AgentSession::new("sess-conflict", "claude-code", "Claude Code");
    session.view_name = "dev".to_string();
    session.set_model_info("anthropic", "claude-sonnet-4-20250514");
    session.set_boundary_start(baseline);
    let event = TurnEvent::new("sess-conflict", HookType::TurnEnd);
    let options = TurnRecordOptions {
        session: &session,
        event: &event,
        turn_number: 1,
        turn_duration_ms: 1000,
        prompt: Some("resolve the name conflict".to_string()),
    };

    let outcome = match record_turn(repo_root, &options).expect("native record_turn must run") {
        TurnRecordResult::Recorded(outcome) => outcome,
        other => panic!("expected Recorded, got {other:?}"),
    };
    assert!(
        outcome
            .recorded_file_list()
            .iter()
            .any(|path| path == "new.txt"),
        "the resolver must record the name-conflicted path: {:?}",
        outcome.recorded_file_list()
    );

    // The conflict resolves to the side whose bytes the working tree held.
    let repo = atomic_repository::Repository::open_readonly(repo_root).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("new.txt", "dev")
            .unwrap()
            .as_deref(),
        Some(b"from-base\n".as_slice()),
        "the winner is chosen by byte equality, not by timestamp"
    );

    // Session provenance survives in the recorded change's hashed metadata.
    let change = repo.load_change(&outcome.hash).unwrap();
    assert!(
        SessionEnvelope::is_session_envelope(&change.hashed.metadata),
        "the recorded change must carry the session envelope"
    );
}

#[test]
fn scoped_record_keeps_sibling_and_preexisting_files_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = atomic_repository::Repository::init(dir.path()).unwrap();
    let parent = repo.current_view().to_string();
    let mut a = AgentSession::new("child-a", "opencode", "OpenCode");
    let mut b = AgentSession::new("child-b", "opencode", "OpenCode");
    for s in [&mut a, &mut b] {
        s.explicit_record_files = true;
        s.set_parent_view(repo.current_view());
        repo.create_view_from(&s.view_name, &parent).unwrap();
    }
    drop(repo);
    for (name, text) in [
        ("a.txt", "child a"),
        ("b.txt", "child b"),
        ("human.txt", "leave alone"),
    ] {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let mut hashes = Vec::new();
    for (s, file) in [(&a, "a.txt"), (&b, "b.txt")] {
        let event = make_event().with_raw_json(
            serde_json::json!({"record_files":{file:scope::fingerprint(dir.path(),file).unwrap()}}),
        );
        let outcome = record_turn(dir.path(), &make_options(s, &event)).unwrap();
        let crate::record::TurnRecordResult::Recorded(outcome) = &outcome else {
            panic!("expected recorded outcome");
        };
        assert_eq!(outcome.recorded_file_list(), &[file.to_string()]);
        hashes.push(outcome.hash);
    }
    assert_ne!(hashes[0], hashes[1]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("human.txt")).unwrap(),
        "leave alone"
    );
}

#[test]
fn scoped_record_empty_missing_invalid_or_stale_manifest_never_sweeps() {
    let dir = tempfile::tempdir().unwrap();
    let repo = atomic_repository::Repository::init(dir.path()).unwrap();
    let mut session = make_session();
    session.view_name = repo.current_view().to_string();
    session.explicit_record_files = true;
    drop(repo);
    std::fs::write(dir.path().join("unrelated.txt"), "human").unwrap();
    let empty = make_event().with_raw_json(serde_json::json!({"record_files":{}}));
    assert!(matches!(
        record_turn(dir.path(), &make_options(&session, &empty)),
        Err(AgentError::EmptyTurn { .. })
    ));
    for payload in [
        serde_json::json!({}),
        serde_json::json!({"record_files":null}),
        serde_json::json!({"record_files":{"../escape":null}}),
        serde_json::json!({"record_files":{"unrelated.txt":"wrong digest"}}),
        serde_json::json!({"record_files":{".atomic/config.toml":null}}),
    ] {
        let event = make_event().with_raw_json(payload);
        assert!(matches!(
            record_turn(dir.path(), &make_options(&session, &event)),
            Err(AgentError::RecordFailed { .. })
        ));
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("unrelated.txt")).unwrap(),
        "human"
    );
}

#[test]
fn scoped_record_recovers_session_touched_deletions_after_claim_loss() {
    let dir = tempfile::tempdir().unwrap();
    let repo = atomic_repository::Repository::init(dir.path()).unwrap();
    let mut session = make_session();
    session.view_name = repo.current_view().to_string();
    session.explicit_record_files = true;
    drop(repo);
    std::fs::write(dir.path().join("mine.txt"), "recorded by this session").unwrap();
    std::fs::write(
        dir.path().join("foreign.txt"),
        "recorded by another session",
    )
    .unwrap();

    // This session records mine.txt; the recorded paths land in the
    // persisted session state (files_touched), like the orchestrator does.
    let mine_event =
        make_event().with_raw_json(serde_json::json!({"record_files":{"mine.txt":scope::fingerprint(dir.path(),"mine.txt").unwrap()}}));
    let outcome = record_turn(dir.path(), &make_options(&session, &mine_event)).unwrap();
    let outcome = match outcome {
        crate::record::TurnRecordResult::Recorded(outcome) => outcome,
        other => panic!("expected recorded outcome, got {other:?}"),
    };
    session.add_files_touched(outcome.recorded_file_list());

    // Another session records foreign.txt.
    let mut other = AgentSession::new("other-session", "opencode", "OpenCode");
    other.view_name = session.view_name.clone();
    other.explicit_record_files = true;
    let foreign_event = make_event().with_raw_json(
        serde_json::json!({"record_files":{"foreign.txt":scope::fingerprint(dir.path(),"foreign.txt").unwrap()}}),
    );
    record_turn(dir.path(), &make_options(&other, &foreign_event)).unwrap();

    // Both files are deleted on disk. The plugin restarted, so the stop
    // manifest arrives EMPTY — every ownership claim was lost.
    std::fs::remove_file(dir.path().join("mine.txt")).unwrap();
    std::fs::remove_file(dir.path().join("foreign.txt")).unwrap();
    let empty = make_event().with_raw_json(serde_json::json!({"record_files":{}}));

    // mine.txt is still attributable from the session state and records as a
    // deletion instead of stranding; foreign.txt stays out of scope.
    let outcome = record_turn(dir.path(), &make_options(&session, &empty)).unwrap();
    let outcome = match outcome {
        crate::record::TurnRecordResult::Recorded(outcome) => outcome,
        other => panic!("expected recorded outcome, got {other:?}"),
    };
    assert_eq!(outcome.recorded_file_list(), &["mine.txt".to_string()]);

    let repo = atomic_repository::Repository::open_existing(dir.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let status = repo
        .status(
            working_copy,
            atomic_repository::status::StatusOptions::default().with_untracked(true),
        )
        .unwrap();
    let pending: Vec<String> = status
        .entries()
        .iter()
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();
    assert!(!pending.contains(&"mine.txt".to_string()));
    assert!(pending.contains(&"foreign.txt".to_string()));
}

#[test]
fn scoped_snapshot_includes_requested_files_restored_to_clean_and_deletions() {
    let dir = tempfile::tempdir().unwrap();
    drop(atomic_repository::Repository::init(dir.path()).unwrap());
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let first = scope::snapshot(dir.path(), &[]).unwrap();
    assert!(first["files"]["a.txt"].is_string());
    std::fs::remove_file(dir.path().join("a.txt")).unwrap();
    let after = scope::snapshot(dir.path(), &["a.txt".into()]).unwrap();
    assert!(after["files"].as_object().unwrap().contains_key("a.txt"));
    assert!(after["files"]["a.txt"].is_null());
}
