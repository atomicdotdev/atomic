use super::*;

#[allow(clippy::module_inception)]
mod tests {
    use super::*;
    use atomic_core::change::ChangeHeader;
    use atomic_core::change::{Atom, Encoding, Insertion, Local};
    use atomic_core::types::{ChangePosition, Merkle, Position};
    use atomic_core::EdgeFlags;
    use atomic_core::change::{AITool, AIVendor, PromptContent, Provenance, SuggestionType};

    // ChangeFormat Tests

    #[test]
    fn test_change_format_default_is_default() {
        let format = ChangeFormat::default();
        assert_eq!(format, ChangeFormat::Default);
    }

    #[test]
    fn test_change_format_display() {
        assert_eq!(ChangeFormat::Default.to_string(), "default");
        assert_eq!(ChangeFormat::Short.to_string(), "short");
        assert_eq!(ChangeFormat::Json.to_string(), "json");
    }

    #[test]
    fn test_change_format_from_str_default() {
        assert_eq!(
            "default".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Default
        );
        assert_eq!(
            "full".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Default
        );
    }

    #[test]
    fn test_change_format_from_str_short() {
        assert_eq!(
            "short".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Short
        );
    }

    #[test]
    fn test_change_format_from_str_json() {
        assert_eq!("json".parse::<ChangeFormat>().unwrap(), ChangeFormat::Json);
    }

    #[test]
    fn test_change_format_from_str_case_insensitive() {
        assert_eq!(
            "DEFAULT".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Default
        );
        assert_eq!(
            "SHORT".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Short
        );
        assert_eq!("JSON".parse::<ChangeFormat>().unwrap(), ChangeFormat::Json);
    }

    #[test]
    fn test_change_format_from_str_invalid() {
        let result = "invalid".parse::<ChangeFormat>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid format"));
    }

    #[test]
    fn test_change_format_equality() {
        assert_eq!(ChangeFormat::Default, ChangeFormat::Default);
        assert_ne!(ChangeFormat::Default, ChangeFormat::Short);
        assert_ne!(ChangeFormat::Short, ChangeFormat::Json);
    }

    #[test]
    fn test_change_format_clone() {
        let format = ChangeFormat::Short;
        let cloned = format;
        assert_eq!(format, cloned);
    }

    #[test]
    fn test_change_format_copy() {
        let format = ChangeFormat::Json;
        let copied: ChangeFormat = format;
        assert_eq!(format, copied);
    }

    // ChangeIdentifier Tests

    #[test]
    fn test_identifier_parse_none() {
        let id = ChangeIdentifier::parse(None).unwrap();
        assert_eq!(id, ChangeIdentifier::Latest);
    }

    #[test]
    fn test_identifier_parse_empty() {
        let id = ChangeIdentifier::parse(Some("")).unwrap();
        assert_eq!(id, ChangeIdentifier::Latest);
    }

    #[test]
    fn test_identifier_parse_sequence_with_hash() {
        let id = ChangeIdentifier::parse(Some("#42")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(42));
    }

    #[test]
    fn test_identifier_parse_sequence_numeric() {
        let id = ChangeIdentifier::parse(Some("123")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(123));
    }

    #[test]
    fn test_identifier_parse_sequence_zero() {
        let id = ChangeIdentifier::parse(Some("0")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(0));
    }

    #[test]
    fn test_identifier_parse_hash_prefix() {
        let id = ChangeIdentifier::parse(Some("ABCD")).unwrap();
        assert_eq!(id, ChangeIdentifier::HashPrefix("ABCD".to_string()));
    }

    #[test]
    fn test_identifier_parse_hash_prefix_lowercase() {
        let id = ChangeIdentifier::parse(Some("abcdefgh")).unwrap();
        assert_eq!(id, ChangeIdentifier::HashPrefix("ABCDEFGH".to_string()));
    }

    #[test]
    fn test_identifier_parse_hash_prefix_mixed_case() {
        let id = ChangeIdentifier::parse(Some("AbCdEf")).unwrap();
        assert_eq!(id, ChangeIdentifier::HashPrefix("ABCDEF".to_string()));
    }

    #[test]
    fn test_identifier_parse_full_hash() {
        // 52-character base32 hash
        let full_hash = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let id = ChangeIdentifier::parse(Some(full_hash)).unwrap();
        assert!(matches!(id, ChangeIdentifier::FullHash(_)));
    }

    #[test]
    fn test_identifier_parse_prefix_too_short() {
        let result = ChangeIdentifier::parse(Some("ABC"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too short"));
    }

    #[test]
    fn test_identifier_parse_invalid_characters() {
        let result = ChangeIdentifier::parse(Some("ABCD1890")); // 8, 9, 0 are invalid in base32
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid hash characters"));
    }

    #[test]
    fn test_identifier_parse_whitespace_trimmed() {
        let id = ChangeIdentifier::parse(Some("  ABCDEF  ")).unwrap();
        assert_eq!(id, ChangeIdentifier::HashPrefix("ABCDEF".to_string()));
    }

    #[test]
    fn test_identifier_is_latest() {
        assert!(ChangeIdentifier::Latest.is_latest());
        assert!(!ChangeIdentifier::Sequence(0).is_latest());
        assert!(!ChangeIdentifier::HashPrefix("ABC".to_string()).is_latest());
    }

    #[test]
    fn test_identifier_is_sequence() {
        assert!(ChangeIdentifier::Sequence(42).is_sequence());
        assert!(!ChangeIdentifier::Latest.is_sequence());
        assert!(!ChangeIdentifier::HashPrefix("ABC".to_string()).is_sequence());
    }

    #[test]
    fn test_identifier_is_hash() {
        assert!(ChangeIdentifier::HashPrefix("ABC".to_string()).is_hash());
        let hash = Hash::of(b"test");
        assert!(ChangeIdentifier::FullHash(hash).is_hash());
        assert!(!ChangeIdentifier::Latest.is_hash());
        assert!(!ChangeIdentifier::Sequence(42).is_hash());
    }

    // ChangeCmd Builder Tests

    #[test]
    fn test_change_cmd_new() {
        let cmd = ChangeCmd::new();
        assert!(cmd.identifier.is_none());
        assert!(cmd.view.is_none());
        assert_eq!(cmd.format, ChangeFormat::Default);
        assert!(!cmd.show_deps);
        assert!(!cmd.show_hunks);
        assert!(!cmd.full_hash);
    }

    #[test]
    fn test_change_cmd_default() {
        let cmd = ChangeCmd::default();
        assert!(cmd.identifier.is_none());
        assert_eq!(cmd.format, ChangeFormat::Default);
    }

    #[test]
    fn test_change_cmd_with_identifier() {
        let cmd = ChangeCmd::new().with_identifier("ABCDEF");
        assert_eq!(cmd.identifier, Some("ABCDEF".to_string()));
    }

    #[test]
    fn test_change_cmd_with_identifier_string() {
        let cmd = ChangeCmd::new().with_identifier(String::from("12345"));
        assert_eq!(cmd.identifier, Some("12345".to_string()));
    }

    #[test]
    fn test_change_cmd_with_view() {
        let cmd = ChangeCmd::new().with_view("feature");
        assert_eq!(cmd.view, Some("feature".to_string()));
    }

    #[test]
    fn test_change_cmd_with_format() {
        let cmd = ChangeCmd::new().with_format(ChangeFormat::Json);
        assert_eq!(cmd.format, ChangeFormat::Json);
    }

    #[test]
    fn test_change_cmd_with_show_deps() {
        let cmd = ChangeCmd::new().with_show_deps(true);
        assert!(cmd.show_deps);
    }

    #[test]
    fn test_change_cmd_with_show_hunks() {
        let cmd = ChangeCmd::new().with_show_hunks(true);
        assert!(cmd.show_hunks);
    }

    #[test]
    fn test_change_cmd_with_full_hash() {
        let cmd = ChangeCmd::new().with_full_hash(true);
        assert!(cmd.full_hash);
    }

    #[test]
    fn test_change_cmd_builder_chain() {
        let cmd = ChangeCmd::new()
            .with_identifier("ABC123")
            .with_view("main")
            .with_format(ChangeFormat::Short)
            .with_show_deps(true)
            .with_show_hunks(true)
            .with_full_hash(true);

        assert_eq!(cmd.identifier, Some("ABC123".to_string()));
        assert_eq!(cmd.view, Some("main".to_string()));
        assert_eq!(cmd.format, ChangeFormat::Short);
        assert!(cmd.show_deps);
        assert!(cmd.show_hunks);
        assert!(cmd.full_hash);
    }

    #[test]
    fn test_change_cmd_get_hash_length_default() {
        let cmd = ChangeCmd::new();
        assert_eq!(cmd.get_hash_length(), DEFAULT_HASH_LENGTH);
    }

    #[test]
    fn test_change_cmd_get_hash_length_full() {
        let cmd = ChangeCmd::new().with_full_hash(true);
        assert_eq!(cmd.get_hash_length(), 52);
    }

    // JsonAuthor Tests

    #[test]
    fn test_json_author_from_author_with_email() {
        let author = Author::new("Alice", Some("alice@example.com"));
        let json_author = JsonAuthor::from(&author);
        assert_eq!(json_author.name, "Alice");
        assert_eq!(json_author.email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_json_author_from_author_without_email() {
        let author = Author::new("Bob", None::<String>);
        let json_author = JsonAuthor::from(&author);
        assert_eq!(json_author.name, "Bob");
        assert!(json_author.email.is_none());
    }

    #[test]
    fn test_json_author_serialize() {
        let json_author = JsonAuthor {
            name: "Charlie".to_string(),
            email: Some("charlie@test.com".to_string()),
        };
        let json = serde_json::to_string(&json_author).unwrap();
        assert!(json.contains("\"name\":\"Charlie\""));
        assert!(json.contains("\"email\":\"charlie@test.com\""));
    }

    #[test]
    fn test_json_author_serialize_no_email() {
        let json_author = JsonAuthor {
            name: "Dave".to_string(),
            email: None,
        };
        let json = serde_json::to_string(&json_author).unwrap();
        assert!(json.contains("\"name\":\"Dave\""));
        // Email should be skipped
        assert!(!json.contains("email"));
    }

    // JsonHunkSummary Tests

    #[test]
    fn test_json_hunk_summary_with_path() {
        let summary = JsonHunkSummary {
            hunk_type: "FileAdd".to_string(),
            path: Some("src/main.rs".to_string()),
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"hunk_type\":\"FileAdd\""));
        assert!(json.contains("\"path\":\"src/main.rs\""));
    }

    #[test]
    fn test_json_hunk_summary_without_path() {
        let summary = JsonHunkSummary {
            hunk_type: "Edit".to_string(),
            path: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"hunk_type\":\"Edit\""));
        assert!(!json.contains("path"));
    }

    // JsonChange Tests

    fn create_test_change() -> Change {
        Change::new(
            ChangeHeader::builder()
                .message("Test change message")
                .description("This is a description")
                .author(Author::new("Test User", Some("test@example.com")))
                .build(),
            vec![],
            vec![],
            vec![],
        )
    }

    fn test_edit_hunk(path: &str, start: u64, end: u64) -> GraphOp<Hash> {
        let change = Hash::of(format!("{}:{}:{}", path, start, end).as_bytes());
        let inode = Position::new(change, ChangePosition::new(0));
        GraphOp::Edit {
            change: Atom::Insertion(Insertion {
                predecessors: Vec::new(),
                successors: Vec::new(),
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(start),
                end: ChangePosition::new(end),
                inode,
            }),
            local: Local::new(path, 1),
            encoding: Some(Encoding::Utf8),
        }
    }

    #[test]
    fn test_json_change_from_change() {
        let change = create_test_change();
        let hash = Hash::of(b"test change");
        let json_change = JsonChange::from_change(&change, &hash, Some(42));

        assert_eq!(json_change.message, "Test change message");
        assert_eq!(
            json_change.description,
            Some("This is a description".to_string())
        );
        assert_eq!(json_change.authors.len(), 1);
        assert_eq!(json_change.authors[0].name, "Test User");
        assert_eq!(json_change.sequence, Some(42));
        assert!(!json_change.has_provenance);
    }

    #[test]
    fn test_json_change_serialize() {
        let change = create_test_change();
        let hash = Hash::of(b"test change");
        let json_change = JsonChange::from_change(&change, &hash, None);

        let json = serde_json::to_string_pretty(&json_change).unwrap();
        assert!(json.contains("\"message\": \"Test change message\""));
        assert!(json.contains("\"description\": \"This is a description\""));
    }

    // Helper Function Tests

    #[test]
    fn test_truncate_string_no_truncation() {
        assert_eq!(truncate_string("Hello", 10), "Hello");
        assert_eq!(truncate_string("World", 5), "World");
    }

    #[test]
    fn test_truncate_string_exact_length() {
        assert_eq!(truncate_string("Hello", 5), "Hello");
    }

    #[test]
    fn test_truncate_string_with_ellipsis() {
        assert_eq!(truncate_string("Hello, World!", 8), "Hello...");
    }

    #[test]
    fn test_truncate_string_very_short_max() {
        assert_eq!(truncate_string("Hello", 3), "Hel");
        assert_eq!(truncate_string("Hello", 2), "He");
    }

    #[test]
    fn test_truncate_string_empty() {
        assert_eq!(truncate_string("", 5), "");
    }

    #[test]
    fn test_format_author_with_email() {
        let author = Author::new("Alice Smith", Some("alice@example.com"));
        assert_eq!(format_author(&author), "Alice Smith <alice@example.com>");
    }

    #[test]
    fn test_format_author_without_email() {
        let author = Author::new("Bob Jones", None::<String>);
        assert_eq!(format_author(&author), "Bob Jones");
    }

    // Format Output Tests

    #[test]
    fn test_format_short_basic() {
        let cmd = ChangeCmd::new();
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let output = cmd.format_short(&change, &hash, Some(5));

        assert!(output.contains("Test change message"));
        assert!(output.contains("#5"));
        assert!(output.contains("Test User"));
    }

    #[test]
    fn test_format_short_no_sequence() {
        let cmd = ChangeCmd::new();
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let output = cmd.format_short(&change, &hash, None);

        assert!(output.contains("Test change message"));
        assert!(!output.contains("#"));
    }

    #[test]
    fn test_format_json_basic() {
        let cmd = ChangeCmd::new();
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let output = cmd.format_json(&change, &hash, Some(10));

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["message"], "Test change message");
        assert_eq!(parsed["sequence"], 10);
    }

    #[test]
    fn test_format_json_no_sequence() {
        let cmd = ChangeCmd::new();
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let output = cmd.format_json(&change, &hash, None);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.get("sequence").is_none() || parsed["sequence"].is_null());
    }

    #[test]
    fn test_hunk_display_summaries_coalesce_same_path() {
        let hunks = vec![
            test_edit_hunk("src/main.rs", 0, 5),
            test_edit_hunk("src/main.rs", 5, 10),
            test_edit_hunk("src/lib.rs", 10, 15),
        ];

        let summaries = hunk_display_summaries(&hunks);

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].path, "src/lib.rs");
        assert_eq!(summaries[0].info, "(+1 span: new content)");
        assert_eq!(summaries[1].path, "src/main.rs");
        assert_eq!(summaries[1].info, "(2 hunks: 2x +1 span: new content)");
    }

    // Integration Tests

    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TestGuard {
        original_dir: std::path::PathBuf,
        _temp_dir: TempDir,
    }

    impl TestGuard {
        fn new() -> Self {
            let original = env::current_dir().unwrap();
            let temp = TempDir::new().unwrap();
            env::set_current_dir(temp.path()).unwrap();
            Self {
                original_dir: original,
                _temp_dir: temp,
            }
        }
    }

    impl Drop for TestGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.original_dir);
        }
    }

    #[test]
    #[serial]
    fn test_change_run_outside_repository() {
        let _guard = TestGuard::new();

        let cmd = ChangeCmd::new();
        let result = cmd.run();

        assert!(result.is_err());
        match result {
            Err(CliError::RepositoryNotFound { .. }) => {}
            Err(CliError::Internal(_)) => {}
            _ => panic!("Expected RepositoryNotFound or Internal error"),
        }
    }

    #[test]
    #[serial]
    fn test_change_run_empty_repository() {
        let _guard = TestGuard::new();

        // Initialize empty repository
        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new();
        let result = cmd.run();

        // Should fail because no changes recorded
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_invalid_sequence() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_identifier("#999");
        let result = cmd.run();

        // Should fail with out of range
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_nonexistent_view() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new()
            .with_identifier("#0")
            .with_view("nonexistent");
        let result = cmd.run();

        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_json_format() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_format(ChangeFormat::Json);
        let result = cmd.run();

        // Will fail (no changes) but shouldn't panic
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_short_format() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_format(ChangeFormat::Short);
        let result = cmd.run();

        // Will fail (no changes) but shouldn't panic
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_with_show_deps() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_show_deps(true);
        let result = cmd.run();

        // Will fail (no changes) but shouldn't panic
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_with_show_hunks() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_show_hunks(true);
        let result = cmd.run();

        // Will fail (no changes) but shouldn't panic
        assert!(result.is_err());
    }

    // Debug and Clone Tests

    #[test]
    fn test_change_format_debug() {
        let format = ChangeFormat::Default;
        let debug_str = format!("{:?}", format);
        assert_eq!(debug_str, "Default");
    }

    #[test]
    fn test_change_identifier_debug() {
        let id = ChangeIdentifier::Sequence(42);
        let debug_str = format!("{:?}", id);
        assert!(debug_str.contains("Sequence"));
        assert!(debug_str.contains("42"));
    }

    #[test]
    fn test_change_cmd_debug() {
        let cmd = ChangeCmd::new();
        let debug_str = format!("{:?}", cmd);
        assert!(debug_str.contains("ChangeCmd"));
    }

    #[test]
    fn test_change_cmd_clone() {
        let cmd = ChangeCmd::new()
            .with_identifier("ABC")
            .with_format(ChangeFormat::Json);
        let cloned = cmd.clone();

        assert_eq!(cmd.identifier, cloned.identifier);
        assert_eq!(cmd.format, cloned.format);
    }

    #[test]
    fn test_change_identifier_clone() {
        let id = ChangeIdentifier::HashPrefix("ABCD".to_string());
        let cloned = id.clone();
        assert_eq!(id, cloned);
    }

    #[test]
    fn test_json_author_debug() {
        let author = JsonAuthor {
            name: "Test".to_string(),
            email: None,
        };
        let debug_str = format!("{:?}", author);
        assert!(debug_str.contains("JsonAuthor"));
    }

    #[test]
    fn test_json_hunk_summary_debug() {
        let summary = JsonHunkSummary {
            hunk_type: "FileAdd".to_string(),
            path: Some("test.rs".to_string()),
        };
        let debug_str = format!("{:?}", summary);
        assert!(debug_str.contains("JsonHunkSummary"));
    }

    #[test]
    fn test_json_change_debug() {
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let json_change = JsonChange::from_change(&change, &hash, None);
        let debug_str = format!("{:?}", json_change);
        assert!(debug_str.contains("JsonChange"));
    }

    #[test]
    fn test_json_author_clone() {
        let author = JsonAuthor {
            name: "Alice".to_string(),
            email: Some("alice@test.com".to_string()),
        };
        let cloned = author.clone();
        assert_eq!(author.name, cloned.name);
        assert_eq!(author.email, cloned.email);
    }

    #[test]
    fn test_json_hunk_summary_clone() {
        let summary = JsonHunkSummary {
            hunk_type: "Edit".to_string(),
            path: Some("file.rs".to_string()),
        };
        let cloned = summary.clone();
        assert_eq!(summary.hunk_type, cloned.hunk_type);
        assert_eq!(summary.path, cloned.path);
    }

    #[test]
    fn test_json_change_clone() {
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let json_change = JsonChange::from_change(&change, &hash, Some(5));
        let cloned = json_change.clone();
        assert_eq!(json_change.hash, cloned.hash);
        assert_eq!(json_change.sequence, cloned.sequence);
    }

    // Edge Case Tests

    #[test]
    fn test_identifier_parse_large_sequence() {
        let id = ChangeIdentifier::parse(Some("999999999999")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(999999999999));
    }

    #[test]
    fn test_identifier_parse_leading_zeros_numeric() {
        let id = ChangeIdentifier::parse(Some("007")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(7));
    }

    #[test]
    fn test_format_short_multiline_message() {
        let cmd = ChangeCmd::new();
        let change = Change::new(
            ChangeHeader::builder()
                .message("First line\nSecond line\nThird line")
                .author(Author::new("Test", None::<String>))
                .build(),
            vec![],
            vec![],
            vec![],
        );
        let hash = Hash::of(b"test");
        let output = cmd.format_short(&change, &hash, None);

        // Short format should only show first line
        assert!(output.contains("First line"));
        assert!(!output.contains("Second line"));
    }

    #[test]
    fn test_format_short_no_authors() {
        let cmd = ChangeCmd::new();
        let change = Change::new(
            ChangeHeader::builder().message("No author message").build(),
            vec![],
            vec![],
            vec![],
        );
        let hash = Hash::of(b"test");
        let output = cmd.format_short(&change, &hash, None);

        assert!(output.contains("(unknown)"));
    }

    #[test]
    fn test_format_json_with_dependencies() {
        let cmd = ChangeCmd::new();
        let dep_hash = Hash::of(b"dependency");
        let change = Change::new(
            ChangeHeader::builder()
                .message("Change with dependency")
                .build(),
            vec![],
            vec![],
            vec![dep_hash],
        );
        let hash = Hash::of(b"main change");
        let output = cmd.format_json(&change, &hash, None);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed["dependencies"].is_array());
        assert_eq!(parsed["dependencies"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_count_unique_paths_empty() {
        let hunks: Vec<GraphOp<Option<Hash>>> = vec![];
        assert_eq!(count_unique_paths(&hunks), 0);
    }

    #[test]
    fn test_truncate_string_unicode() {
        let result = truncate_string("Hello 世界!", 10);
        assert_eq!(result, "Hello 世界!");

        let result2 = truncate_string("Hello 世界!", 8);
        assert!(result2.ends_with("..."));
    }

    #[test]
    fn test_json_provenance_surfaces_agent_context_fields() {
        let prov = Provenance {
            vendor: AIVendor::Anthropic,
            model: "claude-sonnet-4".to_string(),
            tool: AITool::Cli("claude-code".to_string()),
            suggestion_type: SuggestionType::Complete,
            prompt: PromptContent::hash_from("fix the bug"),
            metadata: vec![("turn_number".to_string(), "2".to_string())],
            agent_mode: Some("build".to_string()),
            finish_reason: Some("stop".to_string()),
            step_count: Some(3),
            session_slug: Some("mighty-rocket".to_string()),
            reasoning_signature: Some("sig-abc".to_string()),
            reasoning_text: Some("thought hard about it".to_string()),
            task_plan: Some("[{\"content\":\"fix\"}]".to_string()),
            ..Default::default()
        };

        let json = JsonProvenance::from(&prov);

        assert_eq!(json.agent_mode.as_deref(), Some("build"));
        assert_eq!(json.finish_reason.as_deref(), Some("stop"));
        assert_eq!(json.step_count, Some(3));
        assert_eq!(json.session_slug.as_deref(), Some("mighty-rocket"));
        assert_eq!(json.reasoning_signature.as_deref(), Some("sig-abc"));
        assert_eq!(json.reasoning_text.as_deref(), Some("thought hard about it"));
        assert!(json.prompt_hash.is_some());
        assert!(json.metadata.is_some());

        // The serialized form carries the keys the gap report measured.
        let value = serde_json::to_value(&json).unwrap();
        for key in [
            "reasoning_text",
            "reasoning_signature",
            "agent_mode",
            "finish_reason",
            "step_count",
            "session_slug",
            "task_plan",
            "prompt_hash",
            "metadata",
        ] {
            assert!(value.get(key).is_some(), "missing key: {}", key);
        }
    }

    #[test]
    fn test_json_provenance_omits_absent_agent_fields() {
        let prov = Provenance {
            vendor: AIVendor::OpenAI,
            model: "gpt-5".to_string(),
            tool: AITool::Cli("codex".to_string()),
            suggestion_type: SuggestionType::Complete,
            ..Default::default()
        };

        let value = serde_json::to_value(JsonProvenance::from(&prov)).unwrap();
        let obj = value.as_object().unwrap();

        // Absent fields stay out of the output entirely.
        for key in ["reasoning_text", "agent_mode", "step_count", "prompt_hash", "metadata"] {
            assert!(!obj.contains_key(key), "unexpected key: {}", key);
        }
        assert_eq!(obj.get("vendor").and_then(|v| v.as_str()), Some("OpenAI"));
    }
}
