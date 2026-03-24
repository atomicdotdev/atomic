use super::*;

impl Record {
    /// Build AI provenance from CLI flags and environment variables.
    ///
    /// Environment variables take precedence over CLI flags for consistency
    /// with AI tool integrations that set environment variables.
    pub(super) fn build_provenance(&self) -> Option<Provenance> {
        // Check if AI-assisted via flag or environment variable
        let ai_enabled = self.ai_assisted
            || std::env::var("ATOMIC_AI_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);

        if !ai_enabled {
            return None;
        }

        // Get provider (required for provenance)
        let provider = self
            .ai_provider
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_PROVIDER").ok());

        let provider = match provider {
            Some(p) => p,
            None => {
                // No provider specified - can't create meaningful provenance
                return None;
            }
        };

        // Get model (required for provenance)
        let model = self
            .ai_model
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_MODEL").ok())
            .unwrap_or_else(|| "unknown".to_string());

        // Parse vendor from provider string
        let vendor = AIVendor::parse(&provider);

        // Get tool type
        let tool_from_env = std::env::var("ATOMIC_AI_TOOL").ok();

        let tool_str = self
            .ai_tool
            .clone()
            .or(tool_from_env)
            .unwrap_or_else(|| "cli".to_string());

        let tool = match tool_str.to_lowercase().as_str() {
            "api" => AITool::Api,
            "chat" => AITool::Chat,
            "cli" => AITool::Cli("atomic".to_string()),
            "ci" => AITool::CI("atomic-ci".to_string()),
            "code-review" | "codereview" => AITool::CodeReview("atomic".to_string()),
            // AI coding assistants - treat as editors/IDEs
            "opencode" => AITool::Editor("opencode".to_string()),
            "cursor" => AITool::Editor("cursor".to_string()),
            "aider" => AITool::Cli("aider".to_string()),
            "claude-code" | "claude_code" => AITool::Cli("claude-code".to_string()),
            other => {
                // Check prefixes FIRST before contains patterns
                // This ensures "cli:opencode" becomes Cli("opencode") not Editor("cli:opencode")
                if other.starts_with("cli:") {
                    AITool::Cli(other.trim_start_matches("cli:").to_string())
                } else if other.starts_with("ci:") {
                    AITool::CI(other.trim_start_matches("ci:").to_string())
                } else if other.starts_with("editor:") {
                    AITool::Editor(other.trim_start_matches("editor:").to_string())
                // Then check for editor or IDE plugin patterns
                } else if other.contains("editor")
                    || other.contains("zed")
                    || other.contains("vscode")
                    || other.contains("vim")
                    || other.contains("emacs")
                {
                    AITool::Editor(other.to_string())
                } else if other.contains("plugin") || other.contains("copilot") {
                    AITool::IdePlugin(other.to_string())
                } else if other.contains("aider") || other.contains("claude-code") {
                    AITool::Cli(other.to_string())
                } else {
                    AITool::Other(other.to_string())
                }
            }
        };

        // Get suggestion type
        let suggestion_str = self
            .ai_suggestion_type
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_SUGGESTION_TYPE").ok())
            .unwrap_or_else(|| "collaborative".to_string());

        let suggestion_type = match suggestion_str.to_lowercase().as_str() {
            "complete" => SuggestionType::Complete,
            "partial" => SuggestionType::Partial,
            "collaborative" => SuggestionType::Collaborative,
            "selection" => SuggestionType::Selection,
            "review" => SuggestionType::Review,
            "documentation" | "docs" => SuggestionType::Documentation,
            "debugging" | "debug" => SuggestionType::Debugging,
            "refactoring" | "refactor" => SuggestionType::Refactoring,
            "testing" | "test" => SuggestionType::Testing,
            _ => SuggestionType::Collaborative,
        };

        // Build the provenance
        let mut builder = Provenance::builder()
            .vendor(vendor)
            .model(&model)
            .tool(tool)
            .suggestion_type(suggestion_type);

        // Add optional fields from CLI or environment
        let input_tokens = self.ai_input_tokens.or_else(|| {
            std::env::var("ATOMIC_AI_INPUT_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        let output_tokens = self.ai_output_tokens.or_else(|| {
            std::env::var("ATOMIC_AI_OUTPUT_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
        });

        if let Some(input) = input_tokens {
            builder = builder.input_tokens(input);
        }
        if let Some(output) = output_tokens {
            builder = builder.output_tokens(output);
        }

        let cost = self.ai_cost_usd.or_else(|| {
            std::env::var("ATOMIC_AI_COST_USD")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        if let Some(cost_usd) = cost {
            builder = builder.cost_usd(cost_usd);
        }

        let request_id = self
            .ai_request_id
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_REQUEST_ID").ok());
        if let Some(req_id) = request_id {
            builder = builder.request_id(req_id);
        }

        let session_id = self
            .ai_session_id
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_SESSION_ID").ok());
        if let Some(sess_id) = session_id {
            builder = builder.session_id(sess_id);
        }

        // Add timestamp
        builder = builder.timestamp(chrono::Utc::now().timestamp());

        Some(builder.build())
    }

    /// Build RecordOptions from command-line arguments.
    pub(super) fn build_options(&self) -> CliResult<RecordOptions> {
        let algorithm = self.parse_algorithm()?;

        let mut options = RecordOptions::new()
            .with_all(self.all)
            .with_algorithm(algorithm)
            .with_skip_binary(self.skip_binary)
            .apply_after_record(!self.dry_run)
            .save_to_store(!self.dry_run);

        // Add specific files if provided
        if !self.files.is_empty() {
            options = options.paths(self.files.clone());
        }

        // Set max size if provided
        if let Some(max_size) = self.max_size {
            options = options.with_max_file_size(max_size);
        }

        // Add AI provenance if enabled
        if let Some(provenance) = self.build_provenance() {
            options = options.add_provenance(provenance);
        }

        Ok(options)
    }
}
