use super::generator::strip_markdown_code_blocks;
use super::*;

// CondensedEntry

#[test]
fn test_entry_user() {
    let e = CondensedEntry::user("Fix the bug");
    assert!(e.is_user());
    assert!(!e.is_assistant());
    assert!(!e.is_tool());
    assert_eq!(e.content.as_deref(), Some("Fix the bug"));
}

#[test]
fn test_entry_assistant() {
    let e = CondensedEntry::assistant("I'll fix it");
    assert!(e.is_assistant());
    assert_eq!(e.content.as_deref(), Some("I'll fix it"));
}

#[test]
fn test_entry_tool() {
    let e = CondensedEntry::tool("Edit", Some("src/main.rs"));
    assert!(e.is_tool());
    assert_eq!(e.tool_name.as_deref(), Some("Edit"));
    assert_eq!(e.tool_detail.as_deref(), Some("src/main.rs"));
}

#[test]
fn test_entry_tool_no_detail() {
    let e = CondensedEntry::tool("Bash", None::<String>);
    assert!(e.is_tool());
    assert!(e.tool_detail.is_none());
}

#[test]
fn test_entry_display_user() {
    let e = CondensedEntry::user("hello");
    assert_eq!(e.to_string(), "[User] hello");
}

#[test]
fn test_entry_display_assistant() {
    let e = CondensedEntry::assistant("response");
    assert_eq!(e.to_string(), "[Assistant] response");
}

#[test]
fn test_entry_display_tool_with_detail() {
    let e = CondensedEntry::tool("Edit", Some("file.rs"));
    assert_eq!(e.to_string(), "[Tool] Edit: file.rs");
}

#[test]
fn test_entry_display_tool_no_detail() {
    let e = CondensedEntry::tool("Bash", None::<String>);
    assert_eq!(e.to_string(), "[Tool] Bash");
}

#[test]
fn test_entry_serde_roundtrip() {
    let entries = vec![
        CondensedEntry::user("prompt"),
        CondensedEntry::assistant("response"),
        CondensedEntry::tool("Edit", Some("file.rs")),
    ];
    let json = serde_json::to_string(&entries).unwrap();
    let parsed: Vec<CondensedEntry> = serde_json::from_str(&json).unwrap();
    assert_eq!(entries, parsed);
}

// Transcript parsing (Claude Code JSONL)

#[test]
fn test_condense_empty() {
    let entries = condense_claude_transcript(b"");
    assert!(entries.is_empty());
}

#[test]
fn test_condense_user_message() {
    let jsonl = br#"{"type":"user","uuid":"1","message":{"content":"Fix the bug"}}"#;
    let entries = condense_claude_transcript(jsonl);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_user());
    assert_eq!(entries[0].content.as_deref(), Some("Fix the bug"));
}

#[test]
fn test_condense_assistant_text() {
    let jsonl = br#"{"type":"assistant","uuid":"2","message":{"content":[{"type":"text","text":"I'll fix it"}]}}"#;
    let entries = condense_claude_transcript(jsonl);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_assistant());
    assert_eq!(entries[0].content.as_deref(), Some("I'll fix it"));
}

#[test]
fn test_condense_tool_use() {
    let jsonl = br#"{"type":"assistant","uuid":"3","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs"}}]}}"#;
    let entries = condense_claude_transcript(jsonl);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_tool());
    assert_eq!(entries[0].tool_name.as_deref(), Some("Edit"));
    assert_eq!(entries[0].tool_detail.as_deref(), Some("src/main.rs"));
}

#[test]
fn test_condense_filters_skill_injection() {
    let jsonl = br#"{"type":"user","uuid":"4","message":{"content":"Base directory for this skill: /path/to/skill...long content..."}}"#;
    let entries = condense_claude_transcript(jsonl);
    assert!(entries.is_empty());
}

#[test]
fn test_condense_minimal_detail_read() {
    let jsonl = br#"{"type":"assistant","uuid":"5","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#;
    let entries = condense_claude_transcript(jsonl);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tool_detail.as_deref(), Some("src/lib.rs"));
}

#[test]
fn test_condense_bash_with_command() {
    let jsonl = br#"{"type":"assistant","uuid":"6","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;
    let entries = condense_claude_transcript(jsonl);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tool_detail.as_deref(), Some("cargo test"));
}

#[test]
fn test_condense_multi_line() {
    let jsonl = b"
{\"type\":\"user\",\"uuid\":\"1\",\"message\":{\"content\":\"Hello\"}}
{\"type\":\"assistant\",\"uuid\":\"2\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Hi\"}]}}
{\"type\":\"assistant\",\"uuid\":\"3\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Edit\",\"input\":{\"file_path\":\"a.rs\"}}]}}
";
    let entries = condense_claude_transcript(jsonl);
    assert_eq!(entries.len(), 3);
    assert!(entries[0].is_user());
    assert!(entries[1].is_assistant());
    assert!(entries[2].is_tool());
}

#[test]
fn test_condense_skips_malformed_lines() {
    let jsonl = b"not json\n{\"type\":\"user\",\"uuid\":\"1\",\"message\":{\"content\":\"valid\"}}\nalso not json";
    let entries = condense_claude_transcript(jsonl);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content.as_deref(), Some("valid"));
}

#[test]
fn test_condense_transcript_unknown_format() {
    let entries = condense_transcript(b"data", "unknown");
    assert!(entries.is_empty());
}

// format_condensed

#[test]
fn test_format_condensed_empty() {
    let text = format_condensed(&[], &[]);
    assert!(text.is_empty());
}

#[test]
fn test_format_condensed_with_entries() {
    let entries = vec![
        CondensedEntry::user("Hello"),
        CondensedEntry::assistant("Hi"),
    ];
    let text = format_condensed(&entries, &[]);
    assert!(text.contains("[User] Hello"));
    assert!(text.contains("[Assistant] Hi"));
}

#[test]
fn test_format_condensed_with_files() {
    let entries = vec![CondensedEntry::user("Fix it")];
    let files = vec!["src/main.rs".to_string()];
    let text = format_condensed(&entries, &files);
    assert!(text.contains("[Files Modified]"));
    assert!(text.contains("- src/main.rs"));
}

// extract_prompts

#[test]
fn test_extract_prompts() {
    let entries = vec![
        CondensedEntry::user("First prompt"),
        CondensedEntry::assistant("Response"),
        CondensedEntry::user("Second prompt"),
        CondensedEntry::tool("Edit", Some("file.rs")),
    ];
    let prompts = extract_prompts(&entries);
    assert_eq!(prompts, vec!["First prompt", "Second prompt"]);
}

#[test]
fn test_extract_prompts_empty() {
    let entries = vec![CondensedEntry::assistant("No user messages")];
    let prompts = extract_prompts(&entries);
    assert!(prompts.is_empty());
}

// aggregate_tool_usage

#[test]
fn test_aggregate_tool_usage() {
    let entries = vec![
        CondensedEntry::tool("Edit", Some("a.rs")),
        CondensedEntry::tool("Edit", Some("b.rs")),
        CondensedEntry::tool("Bash", Some("cargo test")),
        CondensedEntry::tool("Read", Some("c.rs")),
    ];
    let tools = aggregate_tool_usage(&entries);
    assert_eq!(tools.len(), 3);

    let bash = tools.iter().find(|t| t.tool_name == "Bash").unwrap();
    assert_eq!(bash.invocation_count, 1);
    assert!(bash.files_affected.is_empty()); // Bash doesn't modify files

    let edit = tools.iter().find(|t| t.tool_name == "Edit").unwrap();
    assert_eq!(edit.invocation_count, 2);
    assert_eq!(edit.files_affected, vec!["a.rs", "b.rs"]);
}

#[test]
fn test_aggregate_tool_usage_dedup_files() {
    let entries = vec![
        CondensedEntry::tool("Edit", Some("same.rs")),
        CondensedEntry::tool("Edit", Some("same.rs")),
    ];
    let tools = aggregate_tool_usage(&entries);
    let edit = tools.iter().find(|t| t.tool_name == "Edit").unwrap();
    assert_eq!(edit.invocation_count, 2);
    assert_eq!(edit.files_affected, vec!["same.rs"]); // deduplicated
}

#[test]
fn test_aggregate_tool_usage_sorted() {
    let entries = vec![
        CondensedEntry::tool("Zzz", None::<String>),
        CondensedEntry::tool("Aaa", None::<String>),
    ];
    let tools = aggregate_tool_usage(&entries);
    assert_eq!(tools[0].tool_name, "Aaa");
    assert_eq!(tools[1].tool_name, "Zzz");
}

// TurnReasoning

#[test]
fn test_reasoning_empty() {
    let r = TurnReasoning {
        intent: String::new(),
        outcome: String::new(),
        learnings: Learnings::default(),
        friction: Vec::new(),
        open_items: Vec::new(),
    };
    assert!(r.is_empty());
    assert_eq!(r.learning_count(), 0);
    assert!(!r.has_code_learnings());
}

#[test]
fn test_reasoning_with_content() {
    let r = TurnReasoning {
        intent: "Fix the bug".into(),
        outcome: "Bug fixed".into(),
        learnings: Learnings {
            repo: vec!["Uses RS256".into()],
            code: vec![CodeLearning::new("file.rs", Some(42), "Wrong timezone")],
            workflow: vec!["Use --lib flag".into()],
        },
        friction: vec!["Complex middleware".into()],
        open_items: vec!["Refresh endpoint".into()],
    };
    assert!(!r.is_empty());
    assert_eq!(r.learning_count(), 3);
    assert!(r.has_code_learnings());
}

#[test]
fn test_reasoning_display() {
    let r = TurnReasoning {
        intent: "Fix auth".into(),
        outcome: "Fixed".into(),
        learnings: Learnings {
            repo: vec!["RS256".into()],
            code: vec![],
            workflow: vec![],
        },
        friction: vec![],
        open_items: vec![],
    };
    let display = r.to_string();
    assert!(display.contains("Intent: Fix auth"));
    assert!(display.contains("Outcome: Fixed"));
    assert!(display.contains("RS256"));
}

#[test]
fn test_reasoning_serde_roundtrip() {
    let r = TurnReasoning {
        intent: "intent".into(),
        outcome: "outcome".into(),
        learnings: Learnings {
            repo: vec!["repo learning".into()],
            code: vec![CodeLearning::new("f.rs", Some(1), "finding")
                .with_function("main")
                .with_category("bug")],
            workflow: vec!["workflow".into()],
        },
        friction: vec!["friction".into()],
        open_items: vec!["open".into()],
    };
    let json = serde_json::to_string_pretty(&r).unwrap();
    let parsed: TurnReasoning = serde_json::from_str(&json).unwrap();
    assert_eq!(r, parsed);
}

// CodeLearning

#[test]
fn test_code_learning_minimal() {
    let l = CodeLearning::new("file.rs", Some(42), "Found a bug");
    assert_eq!(l.path, "file.rs");
    assert_eq!(l.line, Some(42));
    assert_eq!(l.finding, "Found a bug");
    assert!(l.function.is_none());
    assert!(l.category.is_none());
    assert!(!l.is_anchored());
}

#[test]
fn test_code_learning_builder() {
    let l = CodeLearning::new("file.rs", Some(42), "Bug")
        .with_function("validate_token")
        .with_end_line(50)
        .with_category("security");
    assert_eq!(l.function.as_deref(), Some("validate_token"));
    assert_eq!(l.end_line, Some(50));
    assert_eq!(l.category.as_deref(), Some("security"));
}

#[test]
fn test_code_learning_display() {
    let l = CodeLearning::new("src/auth.rs", Some(42), "Wrong timezone")
        .with_function("validate_token")
        .with_category("bug");
    let s = l.to_string();
    assert_eq!(s, "src/auth.rs:42 (validate_token) — Wrong timezone [bug]");
}

#[test]
fn test_code_learning_display_range() {
    let l = CodeLearning::new("f.rs", Some(10), "Issue").with_end_line(20);
    assert_eq!(l.to_string(), "f.rs:10-20 — Issue");
}

#[test]
fn test_code_learning_display_no_line() {
    let l = CodeLearning::new("f.rs", None, "General finding");
    assert_eq!(l.to_string(), "f.rs — General finding");
}

#[test]
fn test_code_learning_anchored() {
    let mut l = CodeLearning::new("f.rs", Some(1), "test");
    assert!(!l.is_anchored());

    l._anchor = Some(GraphAnchor {
        trunk: Some((47, 0)),
        branches: vec![(47, 12)],
        leaves: vec![(47, 34)],
    });
    assert!(l.is_anchored());
}

#[test]
fn test_code_learning_serde_with_anchor() {
    let l = CodeLearning {
        path: "f.rs".into(),
        line: Some(42),
        end_line: None,
        function: Some("main".into()),
        finding: "test".into(),
        category: Some("bug".into()),
        _anchor: Some(GraphAnchor {
            trunk: Some((47, 0)),
            branches: vec![(47, 12)],
            leaves: vec![(47, 34), (47, 35)],
        }),
    };
    let json = serde_json::to_string(&l).unwrap();
    let parsed: CodeLearning = serde_json::from_str(&json).unwrap();
    assert_eq!(l, parsed);
    assert!(json.contains("_anchor"));
}

#[test]
fn test_code_learning_serde_without_anchor() {
    let l = CodeLearning::new("f.rs", Some(1), "no anchor");
    let json = serde_json::to_string(&l).unwrap();
    assert!(!json.contains("_anchor")); // skip_serializing_if = None
    let parsed: CodeLearning = serde_json::from_str(&json).unwrap();
    assert_eq!(l, parsed);
    assert!(!parsed.is_anchored());
}

// GraphAnchor

#[test]
fn test_anchor_empty() {
    let a = GraphAnchor::empty();
    assert!(!a.is_populated());
}

#[test]
fn test_anchor_with_trunk() {
    let a = GraphAnchor {
        trunk: Some((1, 0)),
        branches: vec![],
        leaves: vec![],
    };
    assert!(a.is_populated());
}

#[test]
fn test_anchor_with_all() {
    let a = GraphAnchor {
        trunk: Some((1, 0)),
        branches: vec![(1, 5), (1, 6)],
        leaves: vec![(1, 10), (1, 11), (1, 12)],
    };
    assert!(a.is_populated());
}

// ToolUseSummary

#[test]
fn test_tool_summary_display() {
    let s = ToolUseSummary::new("Edit", 3, vec!["a.rs".into(), "b.rs".into()]);
    assert_eq!(s.to_string(), "Edit (×3) → a.rs, b.rs");
}

#[test]
fn test_tool_summary_display_no_files() {
    let s = ToolUseSummary::new("Bash", 1, vec![]);
    assert_eq!(s.to_string(), "Bash (×1)");
}

// UnhashedTurnData

#[test]
fn test_unhashed_new() {
    let entries = vec![
        CondensedEntry::user("Hello"),
        CondensedEntry::tool("Edit", Some("a.rs")),
    ];
    let data = UnhashedTurnData::new("sess-1", 3, "jsonl", entries, &["a.rs".into()]);

    assert_eq!(data.session_id, "sess-1");
    assert_eq!(data.turn_number, 3);
    assert_eq!(data.transcript_format, "jsonl");
    assert_eq!(data.entry_count(), 2);
    assert_eq!(data.prompts, vec!["Hello"]);
    assert_eq!(data.tools_used.len(), 1);
    assert!(!data.has_reasoning());
    assert!(!data.is_redacted());
}

#[test]
fn test_unhashed_with_reasoning() {
    let data = UnhashedTurnData::new("s", 1, "jsonl", vec![], &[]).with_reasoning(TurnReasoning {
        intent: "test".into(),
        outcome: "done".into(),
        learnings: Learnings::default(),
        friction: vec![],
        open_items: vec![],
    });
    assert!(data.has_reasoning());
}

#[test]
fn test_unhashed_serde_roundtrip() {
    let entries = vec![
        CondensedEntry::user("prompt"),
        CondensedEntry::assistant("response"),
    ];
    let data = UnhashedTurnData::new("sess-1", 5, "jsonl", entries, &["f.rs".into()])
        .with_reasoning(TurnReasoning {
            intent: "fix".into(),
            outcome: "fixed".into(),
            learnings: Learnings {
                repo: vec!["pattern".into()],
                code: vec![CodeLearning::new("f.rs", Some(1), "finding")],
                workflow: vec![],
            },
            friction: vec!["issue".into()],
            open_items: vec![],
        });

    let json = serde_json::to_string_pretty(&data).unwrap();
    let parsed: UnhashedTurnData = serde_json::from_str(&json).unwrap();
    assert_eq!(data, parsed);
}

// Attach / Extract / Strip

fn make_empty_change() -> atomic_core::change::Change {
    atomic_core::change::Change::empty(atomic_core::change::ChangeHeader::default())
}

#[test]
fn test_attach_and_extract() {
    let mut change = make_empty_change();
    let data = UnhashedTurnData::new("s1", 2, "jsonl", vec![], &[]);

    attach_unhashed(&mut change, &data).unwrap();
    assert!(has_unhashed(&change));

    let extracted = extract_unhashed(&change).unwrap();
    assert_eq!(extracted.session_id, "s1");
    assert_eq!(extracted.turn_number, 2);
}

#[test]
fn test_extract_no_unhashed() {
    let change = make_empty_change();
    assert!(!has_unhashed(&change));
    assert!(extract_unhashed(&change).is_none());
}

#[test]
fn test_strip_unhashed() {
    let mut change = make_empty_change();
    let data = UnhashedTurnData::new(
        "s1",
        1,
        "jsonl",
        vec![CondensedEntry::user("secret prompt")],
        &[],
    )
    .with_reasoning(TurnReasoning {
        intent: "secret intent".into(),
        outcome: "secret outcome".into(),
        learnings: Learnings::default(),
        friction: vec![],
        open_items: vec![],
    });

    attach_unhashed(&mut change, &data).unwrap();
    assert!(has_unhashed(&change));
    assert!(!is_redacted(&change));

    // Strip
    let stripped = strip_unhashed(&mut change);
    assert!(stripped);
    assert!(is_redacted(&change));

    // Verify the stub
    let extracted = extract_unhashed(&change).unwrap();
    assert_eq!(extracted.session_id, "s1");
    assert_eq!(extracted.turn_number, 1);
    assert!(extracted.is_redacted());
    assert!(extracted.condensed_transcript.is_empty());
    assert!(extracted.reasoning.is_none());
    assert!(extracted.prompts.is_empty());
}

#[test]
fn test_strip_no_data_returns_false() {
    let mut change = make_empty_change();
    assert!(!strip_unhashed(&mut change));
}

#[test]
fn test_strip_preserves_hash_conceptually() {
    // The unhashed section doesn't contribute to the change hash.
    // This test verifies that attach/strip doesn't touch the hashed section.
    let mut change = make_empty_change();
    let hash_before = change.hashed.contents_hash;

    let data = UnhashedTurnData::new("s", 1, "jsonl", vec![], &[]);
    attach_unhashed(&mut change, &data).unwrap();
    let hash_after_attach = change.hashed.contents_hash;

    strip_unhashed(&mut change);
    let hash_after_strip = change.hashed.contents_hash;

    assert_eq!(hash_before, hash_after_attach);
    assert_eq!(hash_before, hash_after_strip);
}

// Learnings

#[test]
fn test_learnings_empty() {
    let l = Learnings::default();
    assert!(l.is_empty());
    assert_eq!(l.total(), 0);
}

#[test]
fn test_learnings_total() {
    let l = Learnings {
        repo: vec!["a".into(), "b".into()],
        code: vec![CodeLearning::new("f", None, "c")],
        workflow: vec!["d".into()],
    };
    assert_eq!(l.total(), 4);
    assert!(!l.is_empty());
}

// EntryType

#[test]
fn test_entry_type_display() {
    assert_eq!(EntryType::User.to_string(), "User");
    assert_eq!(EntryType::Assistant.to_string(), "Assistant");
    assert_eq!(EntryType::Tool.to_string(), "Tool");
}

#[test]
fn test_entry_type_serde() {
    let json = serde_json::to_string(&EntryType::User).unwrap();
    assert_eq!(json, "\"user\"");
    let parsed: EntryType = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, EntryType::User);
}

// strip_markdown_code_blocks

#[test]
fn test_strip_no_blocks() {
    let json = r#"{"intent": "test"}"#;
    assert_eq!(strip_markdown_code_blocks(json), json);
}

#[test]
fn test_strip_json_block() {
    let input = "```json\n{\"intent\": \"test\"}\n```";
    assert_eq!(strip_markdown_code_blocks(input), r#"{"intent": "test"}"#);
}

#[test]
fn test_strip_plain_block() {
    let input = "```\n{\"intent\": \"test\"}\n```";
    assert_eq!(strip_markdown_code_blocks(input), r#"{"intent": "test"}"#);
}

#[test]
fn test_strip_with_whitespace() {
    let input = "  ```json\n  {\"intent\": \"test\"}  \n```  ";
    let result = strip_markdown_code_blocks(input);
    assert!(result.contains("intent"));
}

#[test]
fn test_strip_no_closing() {
    // No closing ``` — return trimmed original
    let input = "```json\n{\"intent\": \"test\"}";
    let result = strip_markdown_code_blocks(input);
    assert!(result.contains("intent"));
}

// truncate_for_display

#[test]
fn test_truncate_short() {
    assert_eq!(truncate_for_display("hello", 100), "hello");
}

#[test]
fn test_truncate_long() {
    let long = "a".repeat(300);
    let result = truncate_for_display(&long, 50);
    assert!(result.len() <= 54); // 47 + "..."
    assert!(result.ends_with("..."));
}

#[test]
fn test_truncate_exact() {
    let s = "a".repeat(50);
    assert_eq!(truncate_for_display(&s, 50), s);
}

// ClaudeCliGenerator construction

#[test]
fn test_generator_default() {
    let gen = ClaudeCliGenerator::new();
    assert!(gen.claude_path.is_none());
    assert!(gen.model.is_none());
    assert_eq!(gen.timeout_secs, 60);
    assert_eq!(gen.claude_path(), "claude");
    assert_eq!(gen.model(), "sonnet");
}

#[test]
fn test_generator_with_options() {
    let gen = ClaudeCliGenerator::new()
        .with_claude_path("/usr/local/bin/claude")
        .with_model("opus")
        .with_timeout(120);
    assert_eq!(gen.claude_path(), "/usr/local/bin/claude");
    assert_eq!(gen.model(), "opus");
    assert_eq!(gen.timeout_secs, 120);
}

#[test]
fn test_generator_build_prompt() {
    let gen = ClaudeCliGenerator::new();
    let prompt = gen.build_prompt("[User] Fix the bug\n[Assistant] Done\n");
    assert!(prompt.contains("<transcript>"));
    assert!(prompt.contains("[User] Fix the bug"));
    assert!(prompt.contains("</transcript>"));
    assert!(prompt.contains("Return a JSON object"));
    assert!(prompt.contains("\"function\""));
    assert!(prompt.contains("\"category\""));
}

#[test]
fn test_generator_clean_env_strips_git() {
    // This test verifies the logic, though the actual env vars depend
    // on the test environment
    let env = ClaudeCliGenerator::clean_env();
    for (key, _) in &env {
        assert!(
            !key.starts_with("GIT_"),
            "GIT_ variable should be stripped: {}",
            key
        );
    }
}

#[test]
fn test_generator_debug() {
    let gen = ClaudeCliGenerator::new();
    let debug = format!("{:?}", gen);
    assert!(debug.contains("ClaudeCliGenerator"));
}

// MockGenerator

#[test]
fn test_mock_generator_success() {
    let reasoning = TurnReasoning {
        intent: "test intent".into(),
        outcome: "test outcome".into(),
        learnings: Learnings::default(),
        friction: vec![],
        open_items: vec![],
    };
    let gen = MockGenerator::success(reasoning.clone());
    let result = gen.generate("transcript", &[]).unwrap();
    assert_eq!(result.intent, "test intent");
    assert_eq!(result.outcome, "test outcome");
}

#[test]
fn test_mock_generator_failure() {
    let gen = MockGenerator::failure("LLM unavailable");
    let result = gen.generate("transcript", &[]);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("LLM unavailable"));
}

// ReasoningGenerator trait object safety

#[test]
fn test_generator_is_object_safe() {
    let reasoning = TurnReasoning {
        intent: "i".into(),
        outcome: "o".into(),
        learnings: Learnings::default(),
        friction: vec![],
        open_items: vec![],
    };
    let gen: Box<dyn ReasoningGenerator> = Box::new(MockGenerator::success(reasoning));
    let result = gen.generate("text", &[]);
    assert!(result.is_ok());
}

// try_generate_reasoning (non-fatal wrapper)

#[test]
fn test_try_generate_no_claude() {
    // Claude CLI is unlikely to be at this path
    let gen = ClaudeCliGenerator::new().with_claude_path("/nonexistent/claude");
    assert!(!gen.is_available());
}

// SUMMARIZATION_PROMPT

#[test]
fn test_prompt_has_required_fields() {
    let gen = ClaudeCliGenerator::new();
    let prompt = gen.build_prompt("test");
    // Verify the prompt asks for all the fields we expect in TurnReasoning
    assert!(prompt.contains("\"intent\""));
    assert!(prompt.contains("\"outcome\""));
    assert!(prompt.contains("\"learnings\""));
    assert!(prompt.contains("\"repo\""));
    assert!(prompt.contains("\"code\""));
    assert!(prompt.contains("\"workflow\""));
    assert!(prompt.contains("\"friction\""));
    assert!(prompt.contains("\"open_items\""));
    assert!(prompt.contains("\"path\""));
    assert!(prompt.contains("\"line\""));
    assert!(prompt.contains("\"function\""));
    assert!(prompt.contains("\"finding\""));
    assert!(prompt.contains("\"category\""));
}

#[test]
fn test_prompt_has_security_boundary() {
    let gen = ClaudeCliGenerator::new();
    let prompt = gen.build_prompt("test");
    // The transcript should be wrapped in XML tags for injection protection
    assert!(prompt.contains("<transcript>"));
    assert!(prompt.contains("</transcript>"));
}

// End-to-end: parse prompt response (simulated)

#[test]
fn test_parse_claude_response_format() {
    // Simulate what Claude CLI returns: {"result": "...json..."}
    let reasoning_json = serde_json::json!({
        "intent": "Fix the authentication bug",
        "outcome": "Fixed token validation, tests passing",
        "learnings": {
            "repo": ["Auth uses RS256"],
            "code": [{
                "path": "src/auth.rs",
                "line": 42,
                "function": "validate_token",
                "finding": "Wrong timezone comparison",
                "category": "bug"
            }],
            "workflow": ["cargo test --lib is faster"]
        },
        "friction": ["Complex middleware stack"],
        "open_items": ["Refresh endpoint has same bug"]
    });

    let cli_response = serde_json::json!({
        "result": reasoning_json.to_string()
    });

    // Simulate the parsing pipeline
    let result_str = cli_response["result"].as_str().unwrap();
    let clean_json = strip_markdown_code_blocks(result_str);
    let reasoning: TurnReasoning = serde_json::from_str(&clean_json).unwrap();

    assert_eq!(reasoning.intent, "Fix the authentication bug");
    assert_eq!(reasoning.outcome, "Fixed token validation, tests passing");
    assert_eq!(reasoning.learnings.repo, vec!["Auth uses RS256"]);
    assert_eq!(reasoning.learnings.code.len(), 1);
    assert_eq!(reasoning.learnings.code[0].path, "src/auth.rs");
    assert_eq!(reasoning.learnings.code[0].line, Some(42));
    assert_eq!(
        reasoning.learnings.code[0].function.as_deref(),
        Some("validate_token")
    );
    assert_eq!(
        reasoning.learnings.code[0].finding,
        "Wrong timezone comparison"
    );
    assert_eq!(reasoning.learnings.code[0].category.as_deref(), Some("bug"));
    assert_eq!(reasoning.friction, vec!["Complex middleware stack"]);
    assert_eq!(reasoning.open_items, vec!["Refresh endpoint has same bug"]);
}

#[test]
fn test_parse_claude_response_with_markdown_wrapper() {
    // Claude sometimes wraps JSON in markdown despite instructions
    let inner_json = r#"{"intent":"test","outcome":"done","learnings":{"repo":[],"code":[],"workflow":[]},"friction":[],"open_items":[]}"#;
    let wrapped = format!("```json\n{}\n```", inner_json);

    let cli_response = serde_json::json!({
        "result": wrapped
    });

    let result_str = cli_response["result"].as_str().unwrap();
    let clean_json = strip_markdown_code_blocks(result_str);
    let reasoning: TurnReasoning = serde_json::from_str(&clean_json).unwrap();

    assert_eq!(reasoning.intent, "test");
    assert_eq!(reasoning.outcome, "done");
}

#[test]
fn test_parse_claude_response_missing_optional_fields() {
    // LLM might not include all optional fields on CodeLearning
    let json = r#"{
        "intent": "test",
        "outcome": "done",
        "learnings": {
            "repo": [],
            "code": [{"path": "f.rs", "finding": "something"}],
            "workflow": []
        },
        "friction": [],
        "open_items": []
    }"#;

    let reasoning: TurnReasoning = serde_json::from_str(json).unwrap();
    assert_eq!(reasoning.learnings.code.len(), 1);
    assert_eq!(reasoning.learnings.code[0].path, "f.rs");
    assert!(reasoning.learnings.code[0].line.is_none());
    assert!(reasoning.learnings.code[0].function.is_none());
    assert!(reasoning.learnings.code[0].category.is_none());
}
