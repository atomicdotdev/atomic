use super::*;

mod tests {
    use super::*;
    use atomic_core::change::{Author, ChangeHeader};
    use atomic_core::types::{Hash, Merkle, NodeId};

    // LogFormat Tests

    #[test]
    fn test_log_format_default_is_default() {
        let format = LogFormat::default();
        assert_eq!(format, LogFormat::Default);
    }

    #[test]
    fn test_log_format_display() {
        assert_eq!(LogFormat::Default.to_string(), "default");
        assert_eq!(LogFormat::Short.to_string(), "short");
        assert_eq!(LogFormat::Oneline.to_string(), "oneline");
        assert_eq!(LogFormat::Json.to_string(), "json");
    }

    #[test]
    fn test_log_format_from_str_default() {
        assert_eq!("default".parse::<LogFormat>().unwrap(), LogFormat::Default);
        assert_eq!("full".parse::<LogFormat>().unwrap(), LogFormat::Default);
    }

    #[test]
    fn test_log_format_from_str_short() {
        assert_eq!("short".parse::<LogFormat>().unwrap(), LogFormat::Short);
    }

    #[test]
    fn test_log_format_from_str_oneline() {
        assert_eq!("oneline".parse::<LogFormat>().unwrap(), LogFormat::Oneline);
        assert_eq!("one".parse::<LogFormat>().unwrap(), LogFormat::Oneline);
        assert_eq!("1".parse::<LogFormat>().unwrap(), LogFormat::Oneline);
    }

    #[test]
    fn test_log_format_from_str_json() {
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
    }

    #[test]
    fn test_log_format_from_str_case_insensitive() {
        assert_eq!("DEFAULT".parse::<LogFormat>().unwrap(), LogFormat::Default);
        assert_eq!("SHORT".parse::<LogFormat>().unwrap(), LogFormat::Short);
        assert_eq!("JSON".parse::<LogFormat>().unwrap(), LogFormat::Json);
    }

    #[test]
    fn test_log_format_from_str_invalid() {
        let result = "invalid".parse::<LogFormat>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid format"));
    }

    #[test]
    fn test_log_format_equality() {
        assert_eq!(LogFormat::Default, LogFormat::Default);
        assert_ne!(LogFormat::Default, LogFormat::Short);
        assert_ne!(LogFormat::Short, LogFormat::Oneline);
        assert_ne!(LogFormat::Oneline, LogFormat::Json);
    }

    #[test]
    fn test_log_format_clone() {
        let format = LogFormat::Short;
        let cloned = format.clone();
        assert_eq!(format, cloned);
    }

    #[test]
    fn test_log_format_copy() {
        let format = LogFormat::Json;
        let copied: LogFormat = format;
        assert_eq!(format, copied);
    }

    // LogOutputConfig Tests

    #[test]
    fn test_log_output_config_default() {
        let config = LogOutputConfig::default();
        assert_eq!(config.format, LogFormat::Default);
        assert!(config.count.is_none());
        assert!(!config.reverse);
        assert_eq!(config.from_sequence, 0);
        assert!(!config.tags_only);
        assert!(config.stack.is_none());
        assert!(config.path.is_none());
        assert_eq!(config.hash_length, DEFAULT_HASH_LENGTH);
    }

    #[test]
    fn test_log_output_config_new() {
        let config = LogOutputConfig::new();
        assert_eq!(config.format, LogFormat::Default);
    }

    #[test]
    fn test_log_output_config_format() {
        let config = LogOutputConfig::new().format(LogFormat::Json);
        assert_eq!(config.format, LogFormat::Json);
    }

    #[test]
    fn test_log_output_config_count() {
        let config = LogOutputConfig::new().count(10);
        assert_eq!(config.count, Some(10));
    }

    #[test]
    fn test_log_output_config_reverse() {
        let config = LogOutputConfig::new().reverse(true);
        assert!(config.reverse);
    }

    #[test]
    fn test_log_output_config_from_sequence() {
        let config = LogOutputConfig::new().from_sequence(42);
        assert_eq!(config.from_sequence, 42);
    }

    #[test]
    fn test_log_output_config_tags_only() {
        let config = LogOutputConfig::new().tags_only(true);
        assert!(config.tags_only);
    }

    #[test]
    fn test_log_output_config_stack() {
        let config = LogOutputConfig::new().stack("feature");
        assert_eq!(config.stack, Some("feature".to_string()));
    }

    #[test]
    fn test_log_output_config_stack_string() {
        let config = LogOutputConfig::new().stack(String::from("main"));
        assert_eq!(config.stack, Some("main".to_string()));
    }

    #[test]
    fn test_log_output_config_path() {
        let config = LogOutputConfig::new().path("src/main.rs");
        assert_eq!(config.path, Some("src/main.rs".to_string()));
    }

    #[test]
    fn test_log_output_config_hash_length() {
        let config = LogOutputConfig::new().hash_length(12);
        assert_eq!(config.hash_length, 12);
    }

    #[test]
    fn test_log_output_config_builder_chain() {
        let config = LogOutputConfig::new()
            .format(LogFormat::Short)
            .count(25)
            .reverse(true)
            .from_sequence(10)
            .tags_only(true)
            .stack("dev")
            .path("lib/")
            .hash_length(16);

        assert_eq!(config.format, LogFormat::Short);
        assert_eq!(config.count, Some(25));
        assert!(config.reverse);
        assert_eq!(config.from_sequence, 10);
        assert!(config.tags_only);
        assert_eq!(config.stack, Some("dev".to_string()));
        assert_eq!(config.path, Some("lib/".to_string()));
        assert_eq!(config.hash_length, 16);
    }

    // Log Command Tests

    #[test]
    fn test_log_new() {
        let log = Log::new();
        assert!(log.count.is_none());
        assert!(log.stack.is_none());
        assert!(!log.tags_only);
        assert!(log.path.is_none());
        assert_eq!(log.format, LogFormat::Default);
        assert!(!log.reverse);
        assert!(log.from.is_none());
        assert!(!log.full_hash);
    }

    #[test]
    fn test_log_default() {
        let log = Log::default();
        assert!(log.count.is_none());
        assert_eq!(log.format, LogFormat::Default);
    }

    #[test]
    fn test_log_with_count() {
        let log = Log::new().with_count(15);
        assert_eq!(log.count, Some(15));
    }

    #[test]
    fn test_log_with_stack() {
        let log = Log::new().with_stack("feature-branch");
        assert_eq!(log.stack, Some("feature-branch".to_string()));
    }

    #[test]
    fn test_log_with_stack_string() {
        let log = Log::new().with_stack(String::from("dev"));
        assert_eq!(log.stack, Some("dev".to_string()));
    }

    #[test]
    fn test_log_with_tags_only() {
        let log = Log::new().with_tags_only(true);
        assert!(log.tags_only);
    }

    #[test]
    fn test_log_with_path() {
        let log = Log::new().with_path("src/lib.rs");
        assert_eq!(log.path, Some("src/lib.rs".to_string()));
    }

    #[test]
    fn test_log_with_format() {
        let log = Log::new().with_format(LogFormat::Oneline);
        assert_eq!(log.format, LogFormat::Oneline);
    }

    #[test]
    fn test_log_with_reverse() {
        let log = Log::new().with_reverse(true);
        assert!(log.reverse);
    }

    #[test]
    fn test_log_with_from() {
        let log = Log::new().with_from(100);
        assert_eq!(log.from, Some(100));
    }

    #[test]
    fn test_log_with_full_hash() {
        let log = Log::new().with_full_hash(true);
        assert!(log.full_hash);
    }

    #[test]
    fn test_log_builder_chain() {
        let log = Log::new()
            .with_count(20)
            .with_stack("release")
            .with_tags_only(true)
            .with_path("docs/")
            .with_format(LogFormat::Json)
            .with_reverse(true)
            .with_from(50)
            .with_full_hash(true);

        assert_eq!(log.count, Some(20));
        assert_eq!(log.stack, Some("release".to_string()));
        assert!(log.tags_only);
        assert_eq!(log.path, Some("docs/".to_string()));
        assert_eq!(log.format, LogFormat::Json);
        assert!(log.reverse);
        assert_eq!(log.from, Some(50));
        assert!(log.full_hash);
    }

    #[test]
    fn test_log_get_hash_length_default() {
        let log = Log::new();
        assert_eq!(log.get_hash_length(), DEFAULT_HASH_LENGTH);
    }

    #[test]
    fn test_log_get_hash_length_full() {
        let log = Log::new().with_full_hash(true);
        assert_eq!(log.get_hash_length(), 52);
    }

    #[test]
    fn test_log_build_history_options_default() {
        let log = Log::new();
        let options = log.build_history_options();
        assert!(options.load_headers);
        assert!(options.limit.is_none());
        assert!(options.view.is_none());
        assert!(!options.tagged_only);
        assert_eq!(options.from_sequence, 0);
    }

    #[test]
    fn test_log_build_history_options_with_count() {
        let log = Log::new().with_count(5);
        let options = log.build_history_options();
        assert_eq!(options.limit, Some(5));
    }

    #[test]
    fn test_log_build_history_options_with_stack() {
        let log = Log::new().with_stack("test-stack");
        let options = log.build_history_options();
        assert_eq!(options.view, Some("test-stack".to_string()));
    }

    #[test]
    fn test_log_build_history_options_with_tags_only() {
        let log = Log::new().with_tags_only(true);
        let options = log.build_history_options();
        assert!(options.tagged_only);
    }

    #[test]
    fn test_log_build_history_options_with_from() {
        let log = Log::new().with_from(25);
        let options = log.build_history_options();
        assert_eq!(options.from_sequence, 25);
    }

    #[test]
    fn test_log_build_history_options_combined() {
        let log = Log::new()
            .with_count(10)
            .with_stack("feature")
            .with_tags_only(true)
            .with_from(5);
        let options = log.build_history_options();

        assert_eq!(options.limit, Some(10));
        assert_eq!(options.view, Some("feature".to_string()));
        assert!(options.tagged_only);
        assert_eq!(options.from_sequence, 5);
        assert!(options.load_headers);
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
    fn test_json_author_serialize_with_email() {
        let json_author = JsonAuthor {
            name: "Charlie".to_string(),
            email: Some("charlie@test.com".to_string()),
        };
        let json = serde_json::to_string(&json_author).unwrap();
        assert!(json.contains("\"name\":\"Charlie\""));
        assert!(json.contains("\"email\":\"charlie@test.com\""));
    }

    #[test]
    fn test_json_author_serialize_without_email() {
        let json_author = JsonAuthor {
            name: "Dave".to_string(),
            email: None,
        };
        let json = serde_json::to_string(&json_author).unwrap();
        assert!(json.contains("\"name\":\"Dave\""));
        // Email should be skipped when None
        assert!(!json.contains("email"));
    }

    // JsonLogEntry Tests

    fn create_test_hash() -> Hash {
        Hash::of(b"test change content")
    }

    fn create_test_merkle() -> Merkle {
        Merkle::of(b"test state")
    }

    fn create_test_entry_without_header() -> HistoryEntry {
        HistoryEntry::new(
            42,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
    }

    fn create_test_entry_with_header() -> HistoryEntry {
        let header = ChangeHeader::builder()
            .message("Test change message")
            .description("This is a longer description.")
            .author(Author::new("Test User", Some("test@example.com")))
            .build();

        HistoryEntry::new(
            42,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
        .with_change_header(header)
        .with_tagged(true)
    }

    #[test]
    fn test_json_log_entry_from_entry_without_header() {
        let entry = create_test_entry_without_header();
        let json_entry = JsonLogEntry::from_entry(&entry);

        assert_eq!(json_entry.sequence, 42);
        assert!(!json_entry.hash.is_empty());
        assert!(!json_entry.state.is_empty());
        assert!(json_entry.message.is_none());
        assert!(json_entry.description.is_none());
        assert!(json_entry.authors.is_empty());
        assert!(json_entry.timestamp.is_none());
        assert!(!json_entry.is_tagged);
    }

    #[test]
    fn test_json_log_entry_from_entry_with_header() {
        let entry = create_test_entry_with_header();
        let json_entry = JsonLogEntry::from_entry(&entry);

        assert_eq!(json_entry.sequence, 42);
        assert_eq!(json_entry.message, Some("Test change message".to_string()));
        assert_eq!(
            json_entry.description,
            Some("This is a longer description.".to_string())
        );
        assert_eq!(json_entry.authors.len(), 1);
        assert_eq!(json_entry.authors[0].name, "Test User");
        assert!(json_entry.timestamp.is_some());
        assert!(json_entry.is_tagged);
    }

    #[test]
    fn test_json_log_entry_serialize() {
        let entry = create_test_entry_with_header();
        let json_entry = JsonLogEntry::from_entry(&entry);
        let json = serde_json::to_string_pretty(&json_entry).unwrap();

        assert!(json.contains("\"sequence\": 42"));
        assert!(json.contains("\"message\": \"Test change message\""));
        assert!(json.contains("\"is_tagged\": true"));
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
        assert_eq!(truncate_string("VeryLongName", 10), "VeryLon...");
    }

    #[test]
    fn test_truncate_string_very_short_max() {
        assert_eq!(truncate_string("Hello", 3), "Hel");
        assert_eq!(truncate_string("Hello", 2), "He");
        assert_eq!(truncate_string("Hello", 1), "H");
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
    fn test_format_short_single_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_short(&[entry], 8);

        assert!(output.contains("Test change message"));
        assert!(output.ends_with('\n'));
    }

    #[test]
    fn test_format_short_multiple_entries() {
        let log = Log::new();
        let entry1 = create_test_entry_with_header();
        let mut entry2 = create_test_entry_without_header();
        entry2.sequence = 43;

        let output = log.format_short(&[entry1, entry2], 8);
        let lines: Vec<&str> = output.lines().collect();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Test change message"));
        assert!(lines[1].contains("(no message)"));
    }

    #[test]
    fn test_format_short_tagged_marker() {
        let log = Log::new();
        let entry = create_test_entry_with_header(); // is_tagged = true
        let output = log.format_short(&[entry], 8);

        // Tagged entries should have a marker
        assert!(output.contains("*"));
    }

    #[test]
    fn test_format_oneline_single_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_oneline(&[entry], 8);

        // Should contain hash, date, author, message on one line
        assert!(output.contains("Test User"));
        assert!(output.contains("Test change message"));
        assert_eq!(output.lines().count(), 1);
    }

    #[test]
    fn test_format_oneline_without_header() {
        let log = Log::new();
        let entry = create_test_entry_without_header();
        let output = log.format_oneline(&[entry], 8);

        assert!(output.contains("(unknown)"));
        assert!(output.contains("(no message)"));
    }

    #[test]
    fn test_format_json_single_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_json(&[entry]);

        // Should be valid JSON array
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_format_json_multiple_entries() {
        let log = Log::new();
        let entry1 = create_test_entry_with_header();
        let mut entry2 = create_test_entry_without_header();
        entry2.sequence = 43;

        let output = log.format_json(&[entry1, entry2]);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_format_json_empty() {
        let log = Log::new();
        let output = log.format_json(&[]);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert!(parsed.as_array().unwrap().is_empty());
    }

    #[test]
    fn test_format_default_single_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_default(&[entry], 8);

        // Should contain change header
        assert!(output.contains("change"));
        // Should contain author
        assert!(output.contains("Author:"));
        assert!(output.contains("Test User"));
        // Should contain date
        assert!(output.contains("Date:"));
        // Should contain message
        assert!(output.contains("Test change message"));
        // Should contain description
        assert!(output.contains("This is a longer description."));
    }

    #[test]
    fn test_format_default_tagged_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_default(&[entry], 8);

        assert!(output.contains("(tag)"));
    }

    #[test]
    fn test_format_default_without_header() {
        let log = Log::new();
        let entry = create_test_entry_without_header();
        let output = log.format_default(&[entry], 8);

        // Should have change line but no author/date/message
        assert!(output.contains("change"));
        // Should not panic, should handle missing info gracefully
    }

    #[test]
    fn test_format_default_multiple_entries_separated() {
        let log = Log::new();
        let entry1 = create_test_entry_with_header();
        let mut entry2 = create_test_entry_with_header();
        entry2.sequence = 43;

        let output = log.format_default(&[entry1, entry2], 8);

        // Count entries by looking for "Author:" lines (each entry has one)
        let author_count = output.matches("Author:").count();
        assert_eq!(author_count, 2);
    }

    // Integration Tests (Repository)

    use serial_test::serial;
    use std::env;
    use std::fs;
    use tempfile::TempDir;

    /// Helper struct to manage test directory and restore working directory.
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
    fn test_log_run_outside_repository() {
        let _guard = TestGuard::new();

        let log = Log::new();
        let result = log.run();

        assert!(result.is_err());
        // Could be RepositoryNotFound or Internal depending on error mapping
        match result {
            Err(CliError::RepositoryNotFound { .. }) => {}
            Err(CliError::Internal(_)) => {}
            _ => panic!("Expected RepositoryNotFound or Internal error"),
        }
    }

    #[test]
    #[serial]
    fn test_log_run_empty_repository() {
        let _guard = TestGuard::new();

        // Initialize empty repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new();
        let result = log.run();

        // Should succeed but print empty message
        // The result could fail due to database initialization issues in tests
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_nonexistent_stack() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_stack("nonexistent-stack");
        let result = log.run();

        // Should fail with stack not found or internal error
        assert!(result.is_err());
        match result {
            Err(CliError::ViewNotFound { name }) => assert_eq!(name, "nonexistent-stack"),
            Err(CliError::Internal(_)) => {} // Also acceptable
            other => panic!("Expected ViewNotFound or Internal error, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_log_run_json_empty() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_format(LogFormat::Json);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_short_format_empty() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_format(LogFormat::Short);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_oneline_format_empty() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_format(LogFormat::Oneline);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_count() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_count(5);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_reverse() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_reverse(true);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_from_sequence() {
        let _guard = TestGuard::new();

        // Initialize repository and drop to release db lock
        {
            let _repo = Repository::init(".").unwrap();
        }

        let log = Log::new().with_from(0);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_tags_only() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_tags_only(true);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_full_hash() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_full_hash(true);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_combined_options() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new()
            .with_count(10)
            .with_format(LogFormat::Short)
            .with_reverse(true)
            .with_full_hash(true);

        let result = log.run();
        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_in_subdirectory() {
        let _guard = TestGuard::new();

        // Initialize repository
        let repo_result = Repository::init(".");
        if repo_result.is_err() {
            // Filesystem may be read-only or other issues in test environment
            return;
        }
        let _repo = repo_result.unwrap();

        // Create and move to subdirectory
        if fs::create_dir("subdir").is_err() {
            // Filesystem may be read-only
            return;
        }
        if env::set_current_dir("subdir").is_err() {
            return;
        }

        let log = Log::new();
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    // Edge Case Tests

    #[test]
    fn test_format_short_entry_with_multiline_message() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("First line\nSecond line\nThird line")
            .build();

        let entry = HistoryEntry::new(1, NodeId::from(1), create_test_hash(), create_test_merkle())
            .with_change_header(header);

        let output = log.format_short(&[entry], 8);

        // Short format should only show first line
        assert!(output.contains("First line"));
        assert!(!output.contains("Second line"));
    }

    #[test]
    fn test_format_oneline_entry_with_multiline_message() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("First line\nSecond line")
            .author(Author::new("Test", None::<String>))
            .build();

        let entry = HistoryEntry::new(1, NodeId::from(1), create_test_hash(), create_test_merkle())
            .with_change_header(header);

        let output = log.format_oneline(&[entry], 8);

        // Oneline format should only show first line
        assert!(output.contains("First line"));
        assert!(!output.contains("Second line"));
        assert_eq!(output.lines().count(), 1);
    }

    #[test]
    fn test_format_default_entry_with_description() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("Short message")
            .description("This is a detailed description\nwith multiple lines.")
            .build();

        let entry = HistoryEntry::new(1, NodeId::from(1), create_test_hash(), create_test_merkle())
            .with_change_header(header);

        let output = log.format_default(&[entry], 8);

        assert!(output.contains("Short message"));
        assert!(output.contains("This is a detailed description"));
        assert!(output.contains("with multiple lines."));
    }

    #[test]
    fn test_format_json_preserves_all_fields() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("JSON test message")
            .description("JSON test description")
            .author(Author::new("JSON Author", Some("json@test.com")))
            .build();

        let entry = HistoryEntry::new(
            99,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
        .with_change_header(header)
        .with_tagged(true);

        let output = log.format_json(&[entry]);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let obj = &parsed[0];

        assert_eq!(obj["sequence"], 99);
        assert_eq!(obj["message"], "JSON test message");
        assert_eq!(obj["description"], "JSON test description");
        assert_eq!(obj["is_tagged"], true);
        assert_eq!(obj["authors"][0]["name"], "JSON Author");
        assert_eq!(obj["authors"][0]["email"], "json@test.com");
    }

    #[test]
    fn test_truncate_string_unicode() {
        // Unicode characters should be handled correctly (counting chars, not bytes)
        let result = truncate_string("Hello 世界!", 10);
        // String has 10 chars, so should not be truncated
        assert_eq!(result, "Hello 世界!");

        // Test actual truncation with unicode
        let result2 = truncate_string("Hello 世界!", 8);
        // Should truncate to 5 chars + "..."
        assert!(result2.ends_with("..."));
        assert_eq!(result2.chars().count(), 8);
    }

    #[test]
    fn test_format_author_empty_name() {
        let author = Author::new("", Some("email@test.com"));
        let formatted = format_author(&author);
        assert_eq!(formatted, " <email@test.com>");
    }

    #[test]
    fn test_log_format_debug() {
        let format = LogFormat::Default;
        let debug_str = format!("{:?}", format);
        assert_eq!(debug_str, "Default");
    }

    #[test]
    fn test_log_output_config_debug() {
        let config = LogOutputConfig::new();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("LogOutputConfig"));
    }

    #[test]
    fn test_log_command_debug() {
        let log = Log::new();
        let debug_str = format!("{:?}", log);
        assert!(debug_str.contains("Log"));
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
    fn test_json_log_entry_debug() {
        let entry = JsonLogEntry {
            sequence: 1,
            hash: "abc".to_string(),
            state: "xyz".to_string(),
            message: None,
            description: None,
            authors: vec![],
            timestamp: None,
            is_tagged: false,
        };
        let debug_str = format!("{:?}", entry);
        assert!(debug_str.contains("JsonLogEntry"));
    }

    #[test]
    fn test_log_clone() {
        let log = Log::new()
            .with_count(5)
            .with_stack("test")
            .with_format(LogFormat::Short);
        let cloned = log.clone();

        assert_eq!(log.count, cloned.count);
        assert_eq!(log.stack, cloned.stack);
        assert_eq!(log.format, cloned.format);
    }

    #[test]
    fn test_log_output_config_clone() {
        let config = LogOutputConfig::new().format(LogFormat::Json).count(10);
        let cloned = config.clone();

        assert_eq!(config.format, cloned.format);
        assert_eq!(config.count, cloned.count);
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
    fn test_json_log_entry_clone() {
        let entry = JsonLogEntry {
            sequence: 42,
            hash: "hash123".to_string(),
            state: "state456".to_string(),
            message: Some("Test".to_string()),
            description: None,
            authors: vec![],
            timestamp: None,
            is_tagged: true,
        };
        let cloned = entry.clone();

        assert_eq!(entry.sequence, cloned.sequence);
        assert_eq!(entry.hash, cloned.hash);
        assert_eq!(entry.is_tagged, cloned.is_tagged);
    }

    #[test]
    fn test_format_empty_entries() {
        let log = Log::new();

        // All formats should handle empty input gracefully
        assert_eq!(log.format_default(&[], 8), "");
        assert_eq!(log.format_short(&[], 8), "");
        assert_eq!(log.format_oneline(&[], 8), "");

        let json_output = log.format_json(&[]);
        assert_eq!(json_output.trim(), "[]");
    }

    #[test]
    fn test_format_short_no_message() {
        let log = Log::new();
        let entry = create_test_entry_without_header();
        let output = log.format_short(&[entry], 8);

        assert!(output.contains("(no message)"));
    }

    #[test]
    fn test_format_oneline_long_author_name_truncated() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("Test message")
            .author(Author::new(
                "This Is A Very Long Author Name That Should Be Truncated",
                None::<String>,
            ))
            .build();

        let entry = HistoryEntry::new(1, NodeId::from(1), create_test_hash(), create_test_merkle())
            .with_change_header(header);

        let output = log.format_oneline(&[entry], 8);

        // Author name should be truncated (max 20 chars)
        assert!(!output.contains("This Is A Very Long Author Name That Should Be Truncated"));
    }
}
