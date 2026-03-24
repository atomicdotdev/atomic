use super::*;

impl Record {
    /// Create a new Record command with default settings.
    pub fn new() -> Self {
        Self {
            message: None,
            all: false,
            files: Vec::new(),
            author: None,
            identity: None,
            usage: None,
            edit: false,
            algorithm: "myers".to_string(),
            dry_run: false,
            skip_binary: false,
            max_size: None,
            ai_assisted: false,
            ai_provider: None,
            ai_model: None,
            ai_tool: None,
            ai_suggestion_type: None,
            ai_input_tokens: None,
            ai_output_tokens: None,
            ai_cost_usd: None,
            ai_request_id: None,
            ai_session_id: None,
        }
    }

    /// Parse the algorithm string into an Algorithm enum.
    pub(super) fn parse_algorithm(&self) -> CliResult<Algorithm> {
        match self.algorithm.to_lowercase().as_str() {
            "myers" => Ok(Algorithm::Myers),
            "patience" => Ok(Algorithm::Patience),
            other => Err(CliError::InvalidArgument {
                message: format!(
                    "Invalid algorithm '{}'. Expected 'myers' or 'patience'.",
                    other
                ),
            }),
        }
    }

    /// Parse the author string into an Author struct.
    pub(super) fn parse_author(&self) -> Option<Author> {
        self.author.as_ref().map(|s| {
            // Try to parse "Name <email>" format
            if let Some(bracket_start) = s.find('<') {
                if let Some(bracket_end) = s.find('>') {
                    let name = s[..bracket_start].trim();
                    let email = s[bracket_start + 1..bracket_end].trim();
                    return Author::new(name, Some(email));
                }
            }
            // Just a name
            Author::new(s.trim(), None::<&str>)
        })
    }

    /// Get the author from identity store or command-line override.
    ///
    /// Priority:
    /// 1. --identity flag (specific identity by name)
    /// 2. --author flag (manual override)
    /// 3. --usage flag (default identity for usage context)
    /// 4. Global default identity
    /// 5. Fallback to empty author (let repository handle it)
    pub(super) fn resolve_author(&self) -> CliResult<Option<Author>> {
        // Try to open identity store
        let store = IdentityStore::open_default().ok();

        // 1. If --identity is specified, load that specific identity
        if let Some(identity_name) = &self.identity {
            let store = store.ok_or_else(|| {
                CliError::Internal(anyhow::anyhow!(
                    "Identity store not available. Cannot use --identity flag."
                ))
            })?;

            let identity = store
                .load_by_name(identity_name)
                .map_err(|_| CliError::IdentityNotFound(identity_name.clone()))?;

            return Ok(Some(identity_to_author(&identity)));
        }

        // 2. If --author is specified, use the manual override
        if let Some(author) = self.parse_author() {
            return Ok(Some(author));
        }

        // 3. If --usage is specified, get default for that usage
        if let Some(usage_str) = &self.usage {
            if let Some(ref store) = store {
                let usage = IdentityUsage::parse(usage_str);
                if let Ok(Some(identity)) = store.get_default_for_usage(&usage) {
                    return Ok(Some(identity_to_author(&identity)));
                }
            }
        }

        // 4. Try to get global default identity
        if let Some(ref store) = store {
            if let Ok(Some(identity)) = store.get_default() {
                return Ok(Some(identity_to_author(&identity)));
            }
        }

        // 5. No identity available
        Ok(None)
    }

    /// Get the commit message, potentially from editor.
    pub(super) fn get_message(&self) -> CliResult<String> {
        // If message was provided, use it
        if let Some(ref msg) = self.message {
            return Ok(msg.clone());
        }

        // If edit flag is set or we're in a terminal without a message
        if self.edit {
            return self.get_message_from_editor();
        }

        // Check if stdin is a terminal
        if is_terminal() {
            // Interactive mode - prompt for message
            print!("Enter commit message (empty line to cancel): ");
            std::io::stdout().flush().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to flush stdout: {}", e))
            })?;

            let mut message = String::new();
            std::io::stdin().read_line(&mut message).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to read message: {}", e))
            })?;

            let message = message.trim();
            if message.is_empty() {
                return Err(CliError::Cancelled);
            }

            Ok(message.to_string())
        } else {
            // Non-interactive mode - use default message
            Ok("No message provided".to_string())
        }
    }

    /// Open an editor to get the commit message.
    pub(super) fn get_message_from_editor(&self) -> CliResult<String> {
        // Create a temporary file for the message
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("ATOMIC_EDITMSG");

        // Write template to file
        let template = r#"
# Enter your commit message above.
# Lines starting with '#' will be ignored.
# An empty message aborts the commit.
"#;

        std::fs::write(&temp_file, template).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create temp file: {}", e))
        })?;

        // Get editor from environment
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "vi".to_string()
                }
            });

        // Open editor
        let status = std::process::Command::new(&editor)
            .arg(&temp_file)
            .status()
            .map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to open editor '{}': {}", editor, e))
            })?;

        if !status.success() {
            return Err(CliError::Internal(anyhow::anyhow!(
                "Editor exited with non-zero status: {:?}",
                status.code()
            )));
        }

        // Read the file back
        let content = std::fs::read_to_string(&temp_file)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to read temp file: {}", e)))?;

        // Clean up
        let _ = std::fs::remove_file(&temp_file);

        // Process content - remove comment lines and trim
        let message: String = content
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();

        if message.is_empty() {
            return Err(CliError::Cancelled);
        }

        Ok(message)
    }
}
