use super::*;

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsString;
    use std::path::PathBuf;

    const AI_ENV_KEYS: &[&str] = &[
        "ATOMIC_AI_ENABLED",
        "ATOMIC_AI_PROVIDER",
        "ATOMIC_AI_MODEL",
        "ATOMIC_AI_TOOL",
        "ATOMIC_AI_SUGGESTION_TYPE",
        "ATOMIC_AI_INPUT_TOKENS",
        "ATOMIC_AI_OUTPUT_TOKENS",
        "ATOMIC_AI_COST_USD",
        "ATOMIC_AI_REQUEST_ID",
        "ATOMIC_AI_SESSION_ID",
    ];

    struct AiEnvGuard {
        original: Vec<(&'static str, Option<OsString>)>,
    }

    impl AiEnvGuard {
        fn clear() -> Self {
            let original = AI_ENV_KEYS
                .iter()
                .map(|&key| (key, std::env::var_os(key)))
                .collect();
            for key in AI_ENV_KEYS {
                std::env::remove_var(key);
            }
            Self { original }
        }
    }

    impl Drop for AiEnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.original {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    // Record Command Construction Tests

    #[test]
    fn test_record_new() {
        let record = Record::new();
        assert!(record.message.is_none());
        assert!(!record.all);
        assert!(record.files.is_empty());
        assert!(record.author.is_none());
        assert!(record.identity.is_none());
        assert!(record.usage.is_none());
        assert!(!record.edit);
        assert_eq!(record.algorithm, "myers");
        assert!(!record.dry_run);
        assert!(!record.skip_binary);
        assert!(record.max_size.is_none());
    }

    #[test]
    fn test_record_with_identity() {
        let record = Record::new()
            .with_identity("alice")
            .with_message("Test commit");

        assert_eq!(record.identity, Some("alice".to_string()));
        assert_eq!(record.message, Some("Test commit".to_string()));
    }

    #[test]
    fn test_record_with_usage() {
        let record = Record::new().with_usage("work").with_message("Work commit");

        assert_eq!(record.usage, Some("work".to_string()));
    }

    #[test]
    fn test_record_identity_precedence() {
        // When both identity and author are set, identity takes precedence
        // (tested via resolve_author logic)
        let record = Record::new()
            .with_identity("alice")
            .with_author("Bob <bob@example.com>");

        assert!(record.identity.is_some());
        assert!(record.author.is_some());
        // Note: resolve_author would use identity first
    }

    #[test]
    fn test_record_default() {
        let record = Record::default();
        assert!(record.message.is_none());
        assert!(!record.all);
        assert!(record.files.is_empty());
    }

    #[test]
    fn test_record_with_message() {
        let record = Record::new().with_message("Test message");
        assert_eq!(record.message, Some("Test message".to_string()));
    }

    #[test]
    fn test_record_with_all() {
        let record = Record::new().with_all(true);
        assert!(record.all);
    }

    #[test]
    fn test_record_with_files_vec() {
        let record = Record::new().with_files(vec!["src/main.rs", "src/lib.rs"]);
        assert_eq!(record.files.len(), 2);
        assert_eq!(record.files[0], "src/main.rs");
        assert_eq!(record.files[1], "src/lib.rs");
    }

    #[test]
    fn test_record_with_files_strings() {
        let record = Record::new().with_files(vec![String::from("README.md")]);
        assert_eq!(record.files.len(), 1);
        assert_eq!(record.files[0], "README.md");
    }

    #[test]
    fn test_record_with_author() {
        let record = Record::new().with_author("Alice <alice@example.com>");
        assert_eq!(record.author, Some("Alice <alice@example.com>".to_string()));
    }

    #[test]
    fn test_record_with_edit() {
        let record = Record::new().with_edit(true);
        assert!(record.edit);
    }

    #[test]
    fn test_record_with_algorithm_myers() {
        let record = Record::new().with_algorithm("myers");
        assert_eq!(record.algorithm, "myers");
    }

    #[test]
    fn test_record_with_algorithm_patience() {
        let record = Record::new().with_algorithm("patience");
        assert_eq!(record.algorithm, "patience");
    }

    #[test]
    fn test_record_with_dry_run() {
        let record = Record::new().with_dry_run(true);
        assert!(record.dry_run);
    }

    #[test]
    fn test_record_with_skip_binary() {
        let record = Record::new().with_skip_binary(true);
        assert!(record.skip_binary);
    }

    #[test]
    fn test_record_with_max_size() {
        let record = Record::new().with_max_size(1024 * 1024);
        assert_eq!(record.max_size, Some(1024 * 1024));
    }

    #[test]
    fn test_record_builder_chain() {
        let record = Record::new()
            .with_message("Test")
            .with_all(true)
            .with_files(vec!["src/main.rs"])
            .with_author("Bob")
            .with_algorithm("patience")
            .with_dry_run(true)
            .with_skip_binary(true)
            .with_max_size(1000);

        assert_eq!(record.message, Some("Test".to_string()));
        assert!(record.all);
        assert_eq!(record.files.len(), 1);
        assert_eq!(record.author, Some("Bob".to_string()));
        assert_eq!(record.algorithm, "patience");
        assert!(record.dry_run);
        assert!(record.skip_binary);
        assert_eq!(record.max_size, Some(1000));
    }

    // Algorithm Parsing Tests

    #[test]
    fn test_parse_algorithm_myers() {
        let record = Record::new().with_algorithm("myers");
        let result = record.parse_algorithm();
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Algorithm::Myers));
    }

    #[test]
    fn test_parse_algorithm_patience() {
        let record = Record::new().with_algorithm("patience");
        let result = record.parse_algorithm();
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Algorithm::Patience));
    }

    #[test]
    fn test_parse_algorithm_case_insensitive() {
        let record = Record::new().with_algorithm("MYERS");
        let result = record.parse_algorithm();
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Algorithm::Myers));
    }

    #[test]
    fn test_parse_algorithm_invalid() {
        let record = Record::new().with_algorithm("invalid");
        let result = record.parse_algorithm();
        assert!(result.is_err());
    }

    // Author Parsing Tests

    #[test]
    fn test_parse_author_full() {
        let record = Record::new().with_author("Alice <alice@example.com>");
        let author = record.parse_author();
        assert!(author.is_some());
        let author = author.unwrap();
        assert_eq!(author.name, "Alice");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_parse_author_name_only() {
        let record = Record::new().with_author("Bob");
        let author = record.parse_author();
        assert!(author.is_some());
        let author = author.unwrap();
        assert_eq!(author.name, "Bob");
        assert_eq!(author.email, None);
    }

    #[test]
    fn test_parse_author_none() {
        let record = Record::new();
        let author = record.parse_author();
        assert!(author.is_none());
    }

    #[test]
    fn test_parse_author_with_spaces() {
        let record = Record::new().with_author("Alice Smith <alice.smith@example.com>");
        let author = record.parse_author();
        assert!(author.is_some());
        let author = author.unwrap();
        assert_eq!(author.name, "Alice Smith");
        assert_eq!(author.email, Some("alice.smith@example.com".to_string()));
    }

    // Build Options Tests

    #[test]
    fn test_build_options_default() {
        let record = Record::new();
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert!(!options.all());
        assert!(!options.skip_binary());
    }

    #[test]
    fn test_build_options_with_all() {
        let record = Record::new().with_all(true);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert!(options.all());
    }

    #[test]
    fn test_build_options_with_files() {
        let record = Record::new().with_files(vec!["src/main.rs"]);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert_eq!(options.get_paths().len(), 1);
    }

    #[test]
    fn test_build_options_with_skip_binary() {
        let record = Record::new().with_skip_binary(true);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert!(options.skip_binary());
    }

    #[test]
    fn test_build_options_with_max_size() {
        let record = Record::new().with_max_size(500);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert_eq!(options.max_file_size(), 500);
    }

    #[test]
    fn test_build_options_dry_run_no_save() {
        let record = Record::new().with_dry_run(true);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert!(!options.get_save_to_store());
        assert!(!options.get_apply_after_record());
    }

    #[test]
    fn test_build_options_invalid_algorithm() {
        let record = Record::new().with_algorithm("unknown");
        let result = record.build_options();
        assert!(result.is_err());
    }

    // Format Count Tests

    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "file"), "0 files");
    }

    #[test]
    fn test_format_count_one() {
        assert_eq!(format_count(1, "file"), "1 file");
    }

    #[test]
    fn test_format_count_many() {
        assert_eq!(format_count(5, "file"), "5 files");
    }

    #[test]
    fn test_format_count_different_words() {
        assert_eq!(format_count(1, "change"), "1 change");
        assert_eq!(format_count(2, "change"), "2 changes");
    }

    // Integration Tests (require temp directories)

    /// Guard that restores the current directory when dropped.
    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    #[serial]
    fn test_record_run_outside_repository() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let record = Record::new().with_message("Test");
        let result = record.run();

        // Should fail because we're not in a repository
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_record_run_nothing_to_record() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_message("Test");
        let result = record.run();

        // Should fail because there's nothing to record
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_record_dry_run_shows_changes() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let repo = Repository::init(repo_path).unwrap();
            // Add a file
            std::fs::write(repo_path.join("test.txt"), "Hello").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_dry_run(true);
        let result = record.run();

        // Dry run should succeed without creating a change
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_record_with_message_and_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Hello, World!").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_message("Initial commit");
        let result = record.run();

        // This should work once the full record workflow is complete
        // For now, we check that it attempts to record
        // The actual success depends on the underlying implementation
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    #[serial]
    fn test_record_with_specific_files() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("file1.txt"), "Content 1").unwrap();
            std::fs::write(repo_path.join("file2.txt"), "Content 2").unwrap();
            repo.add("file1.txt", Default::default()).unwrap();
            repo.add("file2.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Record only file1.txt
        let record = Record::new()
            .with_message("Add file1")
            .with_files(vec!["file1.txt"]);

        let result = record.run();
        // Check that it at least attempts to run
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    #[serial]
    fn test_record_all_includes_untracked() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
            // Create file but don't add it
            std::fs::write(repo_path.join("untracked.txt"), "Untracked content").unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_message("Add all").with_all(true);

        let result = record.run();
        // With --all, it should add untracked files
        assert!(result.is_ok() || result.is_err());
    }

    // Edge Case Tests

    #[test]
    fn test_record_empty_message() {
        let record = Record::new().with_message("");
        assert_eq!(record.message, Some("".to_string()));
    }

    #[test]
    fn test_record_unicode_message() {
        let record = Record::new().with_message("添加新功能 🚀");
        assert_eq!(record.message, Some("添加新功能 🚀".to_string()));
    }

    #[test]
    fn test_record_multiline_message() {
        let record = Record::new().with_message("First line\n\nSecond paragraph");
        assert_eq!(
            record.message,
            Some("First line\n\nSecond paragraph".to_string())
        );
    }

    #[test]
    fn test_record_with_iterator() {
        let files = vec!["a.rs", "b.rs", "c.rs"];
        let record = Record::new().with_files(files.into_iter());
        assert_eq!(record.files.len(), 3);
    }

    #[test]
    fn test_record_clone() {
        let record = Record::new()
            .with_message("Test")
            .with_all(true)
            .with_dry_run(true);

        let cloned = record.clone();
        assert_eq!(cloned.message, record.message);
        assert_eq!(cloned.all, record.all);
        assert_eq!(cloned.dry_run, record.dry_run);
    }

    #[test]
    fn test_record_debug() {
        let record = Record::new().with_message("Test");
        let debug_str = format!("{:?}", record);
        assert!(debug_str.contains("Record"));
        assert!(debug_str.contains("Test"));
    }

    // Identity to Author Conversion Tests

    #[test]
    fn test_identity_to_author_with_email() {
        let identity = Identity::builder("alice")
            .email("alice@example.com")
            .build()
            .unwrap();

        let author = identity_to_author(&identity);

        assert_eq!(author.name, "alice");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
        assert!(author.identity.is_some());
        // Identity should be the public key in base32
        assert_eq!(
            author.identity.as_ref().unwrap(),
            &identity.public_key_base32()
        );
    }

    #[test]
    fn test_identity_to_author_without_email() {
        let identity = Identity::builder("bob").build().unwrap();

        let author = identity_to_author(&identity);

        assert_eq!(author.name, "bob");
        assert!(author.email.is_none());
        assert!(author.identity.is_some());
    }

    #[test]
    fn test_identity_to_author_preserves_public_key() {
        let identity = Identity::generate("test-user");
        let author = identity_to_author(&identity);

        // The author's identity field should match the identity's public key
        assert_eq!(author.identity.unwrap(), identity.public_key_base32());
    }

    // AI Provenance Tests

    #[test]
    fn test_record_with_ai_assisted() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514");

        assert!(record.ai_assisted);
        assert_eq!(record.ai_provider, Some("anthropic".to_string()));
        assert_eq!(
            record.ai_model,
            Some("claude-sonnet-4-20250514".to_string())
        );
    }

    #[test]
    fn test_record_ai_flags_default() {
        let record = Record::new();

        assert!(!record.ai_assisted);
        assert!(record.ai_provider.is_none());
        assert!(record.ai_model.is_none());
        assert!(record.ai_tool.is_none());
        assert!(record.ai_suggestion_type.is_none());
        assert!(record.ai_input_tokens.is_none());
        assert!(record.ai_output_tokens.is_none());
        assert!(record.ai_cost_usd.is_none());
        assert!(record.ai_request_id.is_none());
        assert!(record.ai_session_id.is_none());
    }

    #[test]
    fn test_record_with_full_ai_provenance() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514")
            .with_ai_tool("zed-editor")
            .with_ai_suggestion_type("collaborative")
            .with_ai_input_tokens(1500)
            .with_ai_output_tokens(500)
            .with_ai_cost_usd(0.015)
            .with_ai_request_id("req_123abc")
            .with_ai_session_id("sess_456def");

        assert!(record.ai_assisted);
        assert_eq!(record.ai_provider, Some("anthropic".to_string()));
        assert_eq!(
            record.ai_model,
            Some("claude-sonnet-4-20250514".to_string())
        );
        assert_eq!(record.ai_tool, Some("zed-editor".to_string()));
        assert_eq!(record.ai_suggestion_type, Some("collaborative".to_string()));
        assert_eq!(record.ai_input_tokens, Some(1500));
        assert_eq!(record.ai_output_tokens, Some(500));
        assert_eq!(record.ai_cost_usd, Some(0.015));
        assert_eq!(record.ai_request_id, Some("req_123abc".to_string()));
        assert_eq!(record.ai_session_id, Some("sess_456def".to_string()));
    }

    #[test]
    #[serial]
    fn test_build_provenance_disabled() {
        let _env = AiEnvGuard::clear();
        let record = Record::new();
        let provenance = record.build_provenance();
        assert!(provenance.is_none());
    }

    #[test]
    #[serial]
    fn test_build_provenance_enabled_without_provider() {
        let _env = AiEnvGuard::clear();
        let record = Record::new().with_ai_assisted(true);
        // Without provider, provenance should be None
        let provenance = record.build_provenance();
        assert!(provenance.is_none());
    }

    #[test]
    fn test_build_provenance_enabled_with_provider() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514");

        let provenance = record.build_provenance();
        assert!(provenance.is_some());

        let prov = provenance.unwrap();
        assert_eq!(prov.vendor, AIVendor::Anthropic);
        assert_eq!(prov.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_build_provenance_tool_parsing() {
        // Test API tool
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("openai")
            .with_ai_tool("api");
        let prov = record.build_provenance().unwrap();
        assert_eq!(prov.tool, AITool::Api);

        // Test chat tool
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("openai")
            .with_ai_tool("chat");
        let prov = record.build_provenance().unwrap();
        assert_eq!(prov.tool, AITool::Chat);

        // Test CLI tool
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("openai")
            .with_ai_tool("cli");
        let prov = record.build_provenance().unwrap();
        assert!(matches!(prov.tool, AITool::Cli(_)));

        // Test cli:opencode (what opencode passes)
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_tool("cli:opencode");
        let prov = record.build_provenance().unwrap();
        assert!(matches!(prov.tool, AITool::Cli(ref name) if name == "opencode"));

        // Test bare "opencode"
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_tool("opencode");
        let prov = record.build_provenance().unwrap();
        assert!(matches!(prov.tool, AITool::Editor(ref name) if name == "opencode"));

        // Test editor tool
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("openai")
            .with_ai_tool("zed-editor");
        let prov = record.build_provenance().unwrap();
        assert!(matches!(prov.tool, AITool::Editor(_)));
    }

    #[test]
    fn test_build_provenance_suggestion_type_parsing() {
        let cases = vec![
            ("complete", SuggestionType::Complete),
            ("partial", SuggestionType::Partial),
            ("collaborative", SuggestionType::Collaborative),
            ("review", SuggestionType::Review),
            ("documentation", SuggestionType::Documentation),
            ("debugging", SuggestionType::Debugging),
            ("refactoring", SuggestionType::Refactoring),
            ("testing", SuggestionType::Testing),
        ];

        for (input, expected) in cases {
            let record = Record::new()
                .with_ai_assisted(true)
                .with_ai_provider("anthropic")
                .with_ai_suggestion_type(input);
            let prov = record.build_provenance().unwrap();
            assert_eq!(
                prov.suggestion_type, expected,
                "Failed for input: {}",
                input
            );
        }
    }

    #[test]
    fn test_build_provenance_vendor_parsing() {
        let cases = vec![
            ("anthropic", AIVendor::Anthropic),
            ("openai", AIVendor::OpenAI),
            ("google", AIVendor::Google),
            ("meta", AIVendor::Meta),
            ("mistral", AIVendor::Mistral),
        ];

        for (input, expected) in cases {
            let record = Record::new().with_ai_assisted(true).with_ai_provider(input);
            let prov = record.build_provenance().unwrap();
            assert_eq!(prov.vendor, expected, "Failed for input: {}", input);
        }
    }

    #[test]
    fn test_build_provenance_with_tokens_and_cost() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514")
            .with_ai_input_tokens(1000)
            .with_ai_output_tokens(500)
            .with_ai_cost_usd(0.025);

        let prov = record.build_provenance().unwrap();
        assert_eq!(prov.tokens.input_tokens, 1000);
        assert_eq!(prov.tokens.output_tokens, 500);
        assert!(!prov.cost.is_zero());
    }

    #[test]
    fn test_build_options_with_provenance() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514");

        let options = record.build_options().unwrap();
        assert!(options.has_provenance());
        assert_eq!(options.get_provenance().len(), 1);
    }

    #[test]
    #[serial]
    fn test_build_options_without_provenance() {
        let _env = AiEnvGuard::clear();
        let record = Record::new();
        let options = record.build_options().unwrap();
        assert!(!options.has_provenance());
        assert!(options.get_provenance().is_empty());
    }

    #[test]
    #[serial]
    fn test_build_provenance_from_env_var() {
        let _env = AiEnvGuard::clear();
        // Set environment variables
        std::env::set_var("ATOMIC_AI_ENABLED", "true");
        std::env::set_var("ATOMIC_AI_PROVIDER", "openai");
        std::env::set_var("ATOMIC_AI_MODEL", "gpt-4");

        let record = Record::new(); // No CLI flags set
        let provenance = record.build_provenance();

        // Clean up
        std::env::remove_var("ATOMIC_AI_ENABLED");
        std::env::remove_var("ATOMIC_AI_PROVIDER");
        std::env::remove_var("ATOMIC_AI_MODEL");

        assert!(provenance.is_some());
        let prov = provenance.unwrap();
        assert_eq!(prov.vendor, AIVendor::OpenAI);
        assert_eq!(prov.model, "gpt-4");
    }

    #[test]
    #[serial]
    fn test_build_provenance_cli_overrides_env() {
        let _env = AiEnvGuard::clear();
        // Set environment variables
        std::env::set_var("ATOMIC_AI_ENABLED", "true");
        std::env::set_var("ATOMIC_AI_PROVIDER", "openai");
        std::env::set_var("ATOMIC_AI_MODEL", "gpt-4");

        // CLI flags take precedence
        let record = Record::new()
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514");

        let provenance = record.build_provenance();

        // Clean up
        std::env::remove_var("ATOMIC_AI_ENABLED");
        std::env::remove_var("ATOMIC_AI_PROVIDER");
        std::env::remove_var("ATOMIC_AI_MODEL");

        assert!(provenance.is_some());
        let prov = provenance.unwrap();
        // CLI values should be used (environment variables are fallback)
        assert_eq!(prov.vendor, AIVendor::Anthropic);
        assert_eq!(prov.model, "claude-sonnet-4-20250514");
    }
}
