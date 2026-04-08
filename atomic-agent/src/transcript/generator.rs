use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use super::reasoning::TurnReasoning;
use crate::error::{AgentError, AgentResult};

// Reasoning Generator Trait + Claude CLI Implementation

/// The summarization prompt template.
///
/// Wraps the transcript in `<transcript>` tags to provide clear boundary markers.
/// This helps contain any potentially malicious content within the transcript
/// (e.g., prompt injection attempts in user messages or file contents) by giving
/// the LLM a clear structural signal about where the untrusted content begins
/// and ends.
///
/// The prompt asks the LLM to return code learnings with file paths, line numbers,
/// and function names. The `anchor_to_graph()` post-processing step then silently
/// attaches CRDT references from the change's `FileOps`.
const SUMMARIZATION_PROMPT: &str = r#"Analyze this development session transcript and generate a structured summary.

<transcript>
{transcript}
</transcript>

Return a JSON object with this exact structure:
{
  "intent": "What the user was trying to accomplish (1-2 sentences)",
  "outcome": "What was actually achieved (1-2 sentences)",
  "learnings": {
    "repo": ["Codebase-specific patterns, conventions, or gotchas discovered"],
    "code": [{"path": "file/path.rs", "line": 42, "function": "function_name", "finding": "What was learned", "category": "bug|pattern|convention|performance|security"}],
    "workflow": ["General development practices or tool usage insights"]
  },
  "friction": ["Problems, blockers, or annoyances encountered"],
  "open_items": ["Tech debt, unfinished work, or things to revisit later"]
}

Guidelines:
- Be concise but specific
- Include file paths and line numbers for code learnings when the transcript references them
- Include function names when identifiable
- Categorize code learnings: bug, pattern, convention, performance, or security
- Friction should capture both blockers and minor annoyances
- Open items are things intentionally deferred, not failures
- Empty arrays are fine if a category doesn't apply
- Return ONLY the JSON object, no markdown formatting or explanation"#;

/// Default Claude CLI model for summarization.
///
/// Sonnet provides a good balance of quality and cost.
const DEFAULT_MODEL: &str = "sonnet";

/// Trait for generating reasoning summaries from condensed transcripts.
///
/// Implementations can use different backends: Claude CLI, a local model,
/// an API endpoint, or a mock for testing.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_agent::transcript::{ReasoningGenerator, ClaudeCliGenerator};
///
/// let generator = ClaudeCliGenerator::new();
/// let reasoning = generator.generate("condensed transcript text", &["file.rs"])?;
/// println!("Intent: {}", reasoning.intent);
/// ```
pub trait ReasoningGenerator: Send + Sync {
    /// Generate a reasoning summary from a condensed transcript.
    ///
    /// # Arguments
    ///
    /// * `condensed_text` — The formatted condensed transcript
    ///   (`[User]/[Assistant]/[Tool]` format from `format_condensed()`)
    /// * `files` — List of files modified in this turn
    ///
    /// # Returns
    ///
    /// A `TurnReasoning` with intent, outcome, learnings, friction, and open items.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::Internal` if generation fails (LLM not available,
    /// parse error, etc.). Callers should treat this as non-fatal — reasoning
    /// is optional enrichment, not required for recording.
    fn generate(&self, condensed_text: &str, files: &[String]) -> AgentResult<TurnReasoning>;
}

/// Generates reasoning summaries by calling the Claude CLI.
///
/// Runs `claude --print --output-format json` as a subprocess, passing the
/// condensed transcript via stdin. The subprocess is fully isolated from the
/// user's repository to prevent recursive hook triggering and git index pollution.
///
/// # Isolation
///
/// - `cmd.Dir` is set to a temp directory (not the repo)
/// - `GIT_*` environment variables are stripped
/// - `--setting-sources ""` skips all Claude Code settings (hooks, MCP, etc.)
///
/// # Error Handling
///
/// All errors are non-fatal. If Claude CLI is not installed, the subprocess
/// fails, or the response can't be parsed, an error is returned and the caller
/// logs a warning. The change is still recorded without reasoning.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_agent::transcript::{ClaudeCliGenerator, ReasoningGenerator};
///
/// let generator = ClaudeCliGenerator::new();
/// match generator.generate("transcript text", &["file.rs"]) {
///     Ok(reasoning) => println!("Intent: {}", reasoning.intent),
///     Err(e) => eprintln!("Reasoning generation failed (non-fatal): {}", e),
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ClaudeCliGenerator {
    /// Path to the claude CLI executable.
    ///
    /// If `None`, uses `"claude"` (expects it to be in `PATH`).
    pub claude_path: Option<PathBuf>,

    /// Claude model to use for summarization.
    ///
    /// Defaults to `"sonnet"` if not set.
    pub model: Option<String>,

    /// Timeout in seconds for the Claude CLI subprocess.
    ///
    /// Defaults to 60 seconds.
    pub timeout_secs: u64,
}

impl ClaudeCliGenerator {
    /// Create a new generator with default settings.
    pub fn new() -> Self {
        Self {
            claude_path: None,
            model: None,
            timeout_secs: 60,
        }
    }

    /// Set the path to the claude CLI executable.
    #[must_use]
    pub fn with_claude_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.claude_path = Some(path.into());
        self
    }

    /// Set the model to use.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set the timeout in seconds.
    #[must_use]
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Get the effective claude CLI path.
    pub(crate) fn claude_path(&self) -> &str {
        self.claude_path
            .as_ref()
            .and_then(|p| p.to_str())
            .unwrap_or("claude")
    }

    /// Get the effective model name.
    pub(crate) fn model(&self) -> &str {
        self.model.as_deref().unwrap_or(DEFAULT_MODEL)
    }

    /// Build the summarization prompt from the condensed transcript.
    pub(crate) fn build_prompt(&self, condensed_text: &str) -> String {
        SUMMARIZATION_PROMPT.replace("{transcript}", condensed_text)
    }

    /// Strip GIT_* environment variables to prevent the subprocess from
    /// discovering or modifying the parent's git repo.
    pub(crate) fn clean_env() -> Vec<(String, String)> {
        std::env::vars()
            .filter(|(key, _)| !key.starts_with("GIT_"))
            .collect()
    }

    /// Check if the claude CLI is available on PATH.
    pub fn is_available(&self) -> bool {
        Command::new(self.claude_path())
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }
}

impl Default for ClaudeCliGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl ReasoningGenerator for ClaudeCliGenerator {
    fn generate(&self, condensed_text: &str, _files: &[String]) -> AgentResult<TurnReasoning> {
        let prompt = self.build_prompt(condensed_text);
        let claude_path = self.claude_path();
        let model = self.model();

        // Build the command
        // --print: output to stdout instead of interactive mode
        // --output-format json: wrap response in {"result": "..."} JSON
        // --model: which model to use
        // --setting-sources "": skip all settings (hooks, MCP, etc.)
        let mut cmd = Command::new(claude_path);
        cmd.args([
            "--print",
            "--output-format",
            "json",
            "--model",
            model,
            "--setting-sources",
            "",
        ]);

        // Isolate the subprocess from the user's repository
        cmd.current_dir(std::env::temp_dir());
        cmd.env_clear();
        for (key, value) in Self::clean_env() {
            cmd.env(&key, &value);
        }

        // Pass prompt via stdin
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Spawn the process
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AgentError::Internal(format!(
                    "Claude CLI not found at '{}'. Install from https://docs.anthropic.com/en/docs/claude-code",
                    claude_path
                ))
            } else {
                AgentError::Internal(format!("Failed to spawn Claude CLI: {}", e))
            }
        })?;

        // Write prompt to stdin
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes());
            // stdin is dropped here, closing the pipe
        }

        // Wait for the process with timeout
        let output = child
            .wait_with_output()
            .map_err(|e| AgentError::Internal(format!("Claude CLI process failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AgentError::Internal(format!(
                "Claude CLI exited with code {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.trim(),
            )));
        }

        // Parse the CLI JSON response: {"result": "...actual JSON..."}
        let stdout = String::from_utf8_lossy(&output.stdout);
        let cli_response: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| {
            AgentError::Internal(format!(
                "Failed to parse Claude CLI response as JSON: {} (response: {})",
                e,
                truncate_for_error(&stdout, 200),
            ))
        })?;

        // Extract the "result" field which contains the actual summary JSON
        let result_str = cli_response
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AgentError::Internal(format!(
                    "Claude CLI response missing 'result' field: {}",
                    truncate_for_error(&stdout, 200),
                ))
            })?;

        // The result may be wrapped in markdown code blocks — strip them
        let clean_json = strip_markdown_code_blocks(result_str);

        // Parse the summary JSON into TurnReasoning
        let reasoning: TurnReasoning = serde_json::from_str(&clean_json).map_err(|e| {
            AgentError::Internal(format!(
                "Failed to parse reasoning JSON: {} (json: {})",
                e,
                truncate_for_error(&clean_json, 300),
            ))
        })?;

        Ok(reasoning)
    }
}

/// Strip markdown code block wrappers from a JSON string.
///
/// Claude sometimes wraps JSON in ```json ... ``` blocks despite being
/// asked not to. This strips those wrappers.
pub(crate) fn strip_markdown_code_blocks(s: &str) -> String {
    let trimmed = s.trim();

    // Check for ```json ... ``` blocks
    if let Some(rest) = trimmed.strip_prefix("```json") {
        if let Some(idx) = rest.rfind("```") {
            return rest[..idx].trim().to_string();
        }
    }

    // Check for ``` ... ``` blocks
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(idx) = rest.rfind("```") {
            return rest[..idx].trim().to_string();
        }
    }

    trimmed.to_string()
}

/// Truncate a string for display in log messages and UI.
///
/// Adds "..." suffix if truncated. Used for intent/outcome summaries
/// in log output.
pub fn truncate_for_display(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
    format!("{}...", truncated)
}

/// Truncate a string for inclusion in error messages.
pub(crate) fn truncate_for_error(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

/// Generate reasoning from a condensed transcript using the default generator.
///
/// This is a convenience function that creates a `ClaudeCliGenerator` with
/// default settings. For custom configuration, create the generator directly.
///
/// # Arguments
///
/// * `condensed_text` — The formatted condensed transcript
/// * `files` — List of files modified in this turn
///
/// # Returns
///
/// `Ok(TurnReasoning)` on success, `Err` if Claude CLI is not available
/// or generation fails.
pub fn generate_reasoning(condensed_text: &str, files: &[String]) -> AgentResult<TurnReasoning> {
    let generator = ClaudeCliGenerator::new();
    generator.generate(condensed_text, files)
}

/// Generate reasoning if Claude CLI is available, returning None if not.
///
/// This is the non-fatal version — returns `None` instead of an error when
/// Claude CLI is not installed or generation fails. Suitable for the
/// recording pipeline where reasoning is optional enrichment.
pub fn try_generate_reasoning(condensed_text: &str, files: &[String]) -> Option<TurnReasoning> {
    let generator = ClaudeCliGenerator::new();

    if !generator.is_available() {
        log::debug!("Claude CLI not available — skipping reasoning generation");
        return None;
    }

    match generator.generate(condensed_text, files) {
        Ok(reasoning) => {
            if reasoning.is_empty() {
                log::debug!("Reasoning generator returned empty result — skipping");
                None
            } else {
                Some(reasoning)
            }
        }
        Err(e) => {
            log::warn!("Reasoning generation failed (non-fatal): {}", e);
            None
        }
    }
}

// Mock Generator (for testing)

/// A mock reasoning generator for testing.
///
/// Returns a fixed `TurnReasoning` regardless of input, or an error if
/// configured to fail.
#[cfg(test)]
pub struct MockGenerator {
    result: Result<TurnReasoning, String>,
}

#[cfg(test)]
impl MockGenerator {
    /// Create a mock that returns a successful reasoning.
    pub fn success(reasoning: TurnReasoning) -> Self {
        Self {
            result: Ok(reasoning),
        }
    }

    /// Create a mock that returns an error.
    pub fn failure(message: &str) -> Self {
        Self {
            result: Err(message.to_string()),
        }
    }
}

#[cfg(test)]
impl ReasoningGenerator for MockGenerator {
    fn generate(&self, _condensed_text: &str, _files: &[String]) -> AgentResult<TurnReasoning> {
        match &self.result {
            Ok(r) => Ok(r.clone()),
            Err(msg) => Err(AgentError::Internal(msg.clone())),
        }
    }
}
