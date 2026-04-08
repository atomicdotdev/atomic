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
