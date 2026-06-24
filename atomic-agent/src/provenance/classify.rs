//! Rule-based tool call classification.
//!
//! This module provides the deterministic, Tier 1 classifier that maps
//! agent tool calls to [`NodeKind`] values based on tool name, arguments,
//! and output status. It handles the ~80% case without any LLM involvement.
//!
//! The Phase 3 classification layer (LLM-powered) builds on top of this by
//! consolidating sequences of classified tool calls into named `Decision`
//! nodes.
//!
//! # Classification Rules
//!
//! | Tool pattern | NodeKind | Rationale |
//! |-------------|----------|-----------|
//! | read, grep, glob, find_path, list_directory | `Exploration` | Reading/searching codebase |
//! | edit, write, patch, create | `Commitment` | Modifying files on disk |
//! | bash (test/lint/build) | `Verification` | Validating work |
//! | bash (other) | `Execution` | Side effects |
//! | thinking | `Exploration` | Internal reasoning |
//! | Any tool with status="error" | `Error` | Tool failure |
//!
//! # Shell Command Sub-Classification
//!
//! Shell commands (`bash`, `terminal`, `shell`) are sub-classified by
//! inspecting the command string for test, lint, and build patterns:
//!
//! ```text
//! cargo test      → Verification
//! npm run lint    → Verification
//! cargo build     → Verification
//! npm install     → Execution
//! curl ...        → Execution
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::provenance::classify::classify_tool_call;
//! use atomic_agent::provenance::types::NodeKind;
//!
//! assert_eq!(
//!     classify_tool_call("read", None, None, None),
//!     NodeKind::Exploration,
//! );
//!
//! assert_eq!(
//!     classify_tool_call("edit", None, None, None),
//!     NodeKind::Commitment,
//! );
//!
//! assert_eq!(
//!     classify_tool_call("bash", None, None, Some("error")),
//!     NodeKind::Error,
//! );
//! ```

use super::types::NodeKind;

// =============================================================================
// Public API
// =============================================================================

/// Classify a tool call into a [`NodeKind`] based on tool name and arguments.
///
/// This is the primary entry point for the rule-based classifier. It
/// inspects the tool name first, then falls back to argument inspection
/// for ambiguous tools (like `bash`).
///
/// # Arguments
///
/// * `tool_name` — The tool name as reported by the agent (e.g., "read", "edit", "bash").
/// * `tool_input` — The tool's input arguments as a JSON value, if available.
/// * `tool_output` — Truncated tool output text, if available.
/// * `status` — The tool execution status ("completed" or "error"), if available.
///
/// # Returns
///
/// The [`NodeKind`] that best describes this tool call's role in the
/// agent's decision chain.
pub fn classify_tool_call(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    tool_output: Option<&str>,
    status: Option<&str>,
) -> NodeKind {
    // Error status always produces an error node, regardless of tool
    if status == Some("error") {
        return NodeKind::Error;
    }

    let normalized = tool_name.to_lowercase();
    let normalized = normalized.trim();

    match normalized {
        // Read-family tools → Exploration
        "read" | "read_file" | "readfile" | "view" | "cat" => NodeKind::Exploration,

        // Search tools → Exploration
        "grep" | "glob" | "find_path" | "find" | "search" | "ripgrep" | "rg" => {
            NodeKind::Exploration
        }

        // Directory listing → Exploration
        "list_directory" | "listdir" | "ls" | "list" | "tree" => NodeKind::Exploration,

        // Internal reasoning → Exploration
        "thinking" | "think" | "reason" | "plan" => NodeKind::Exploration,

        // Fetch/web tools → Exploration
        "fetch" | "curl" | "web_search" | "browser" => NodeKind::Exploration,

        // Write-family tools → Commitment
        "edit" | "edit_file" | "editfile" => NodeKind::Commitment,
        "write" | "write_file" | "writefile" | "write_to_file" => NodeKind::Commitment,
        "multiedit" | "multi_edit" | "multifile_edit" => NodeKind::Commitment,
        "patch" | "apply_patch" | "apply_diff" => NodeKind::Commitment,
        "create" | "create_file" | "createfile" => NodeKind::Commitment,
        "insert" | "replace" | "delete_file" | "remove_file" => NodeKind::Commitment,

        // Todo/note tools → Commitment (they write structured data)
        "todocreate" | "todowrite" | "todo_create" | "todo_write" => NodeKind::Commitment,

        // Shell commands: inspect the command string to sub-classify
        "bash" | "terminal" | "shell" | "command" | "exec" | "run" => {
            classify_shell_command(tool_input, tool_output)
        }

        // Task/sub-agent → Execution
        "task" | "subagent" | "agent" | "dispatch" | "delegate" => NodeKind::Execution,

        // File operations that are ambiguous — classify by looking at args
        "mv" | "move" | "rename" | "move_path" | "copy" | "copy_path" => NodeKind::Commitment,

        // Diagnostic tools → Exploration
        "diagnostics" | "diagnostic" | "check" => NodeKind::Exploration,

        // Open tools (open file in editor/browser) → Exploration
        "open" => NodeKind::Exploration,

        // Save tools → Commitment
        "save" | "save_file" => NodeKind::Commitment,

        // Unknown → conservative default
        _ => NodeKind::Execution,
    }
}

/// Generate a one-line summary for a tool call node.
///
/// The summary is human-readable and concise, suitable for display in
/// the provenance graph and compaction context.
pub fn summarize_tool_call(
    tool_name: &str,
    kind: NodeKind,
    tool_input: Option<&serde_json::Value>,
    tool_output: Option<&str>,
    status: Option<&str>,
) -> String {
    // For errors, prefix with the tool name and "failed"
    if status == Some("error") {
        let reason = tool_output
            .map(|o| truncate_for_summary(o, 80))
            .unwrap_or_default();
        if reason.is_empty() {
            return format!("{} failed", tool_name);
        }
        return format!("{} failed: {}", tool_name, reason);
    }

    match kind {
        NodeKind::Exploration => summarize_exploration(tool_name, tool_input),
        NodeKind::Commitment => summarize_commitment(tool_name, tool_input),
        NodeKind::Verification => summarize_verification(tool_input, tool_output),
        NodeKind::Execution => summarize_execution(tool_name, tool_input),
        _ => tool_name.to_string(),
    }
}

// =============================================================================
// Shell Command Sub-Classification
// =============================================================================

/// Sub-classify a shell command by inspecting the command string.
///
/// Falls back to `Execution` if the command can't be extracted or doesn't
/// match any known pattern.
fn classify_shell_command(
    tool_input: Option<&serde_json::Value>,
    _tool_output: Option<&str>,
) -> NodeKind {
    let cmd = extract_command(tool_input);
    let cmd = cmd.trim();

    if cmd.is_empty() {
        return NodeKind::Execution;
    }

    // Order matters: check test first (most specific), then lint, then build.
    // Some commands overlap (e.g., `cargo clippy` is both lint and build-ish).
    if is_test_command(cmd) {
        return NodeKind::Verification;
    }
    if is_lint_command(cmd) {
        return NodeKind::Verification;
    }
    if is_typecheck_command(cmd) {
        return NodeKind::Verification;
    }
    if is_build_command(cmd) {
        return NodeKind::Verification;
    }

    // Read-like shell commands → Exploration
    if is_read_command(cmd) {
        return NodeKind::Exploration;
    }

    NodeKind::Execution
}

/// Extract the command string from tool input JSON.
///
/// Tries several common field names: `command`, `cmd`, `script`, `input`.
fn extract_command(tool_input: Option<&serde_json::Value>) -> &str {
    let input = match tool_input {
        Some(v) => v,
        None => return "",
    };

    // Try known field names in order of likelihood
    for field in &["command", "cmd", "script", "input"] {
        if let Some(s) = input.get(field).and_then(|v| v.as_str()) {
            return s;
        }
    }

    // If the input is a bare string, use it directly
    if let Some(s) = input.as_str() {
        return s;
    }

    ""
}

// =============================================================================
// Command Pattern Matchers
// =============================================================================

/// Returns `true` if the command looks like a test invocation.
fn is_test_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let patterns = [
        // Rust
        "cargo test",
        "cargo nextest",
        // JavaScript / TypeScript
        "npm test",
        "npm run test",
        "npx jest",
        "npx vitest",
        "npx mocha",
        "bun test",
        "yarn test",
        "pnpm test",
        "jest",
        "vitest",
        "mocha",
        // Python
        "pytest",
        "python -m pytest",
        "python -m unittest",
        "python -m doctest",
        // Go
        "go test",
        // Ruby
        "rspec",
        "rake test",
        "bundle exec rspec",
        // PHP
        "phpunit",
        "vendor/bin/phpunit",
        // Elixir
        "mix test",
        // .NET
        "dotnet test",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

/// Returns `true` if the command looks like a lint invocation.
fn is_lint_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let patterns = [
        // Rust
        "cargo clippy",
        "cargo fmt -- --check",
        "cargo fmt --check",
        // JavaScript / TypeScript
        "eslint",
        "npx eslint",
        "prettier --check",
        "npx prettier --check",
        "biome check",
        "biome lint",
        "npm run lint",
        "yarn lint",
        "pnpm lint",
        "oxlint",
        // Python
        "pylint",
        "flake8",
        "ruff check",
        "ruff",
        "mypy",
        "pyright",
        // Go
        "golangci-lint",
        "golint",
        "go vet",
        // Ruby
        "rubocop",
        // Shell
        "shellcheck",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

/// Returns `true` if the command looks like a type-check invocation.
fn is_typecheck_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();

    // tsc --noEmit is explicitly a type check (not a build)
    if lower.contains("tsc") && lower.contains("--noemit") {
        return true;
    }
    if lower.contains("tsc") && lower.contains("--noemit") {
        return true;
    }
    if lower.contains("pyright") || lower.contains("mypy") {
        return true;
    }

    false
}

/// Returns `true` if the command looks like a build invocation.
fn is_build_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let patterns = [
        // Rust
        "cargo build",
        "cargo check",
        // JavaScript / TypeScript — tsc alone (without --noEmit) is a build
        "npm run build",
        "yarn build",
        "pnpm build",
        "bun build",
        // Go
        "go build",
        // General
        "make",
        "cmake",
        // .NET
        "dotnet build",
    ];

    // Special case: bare `tsc` without --noEmit is a build
    if lower.contains("tsc") && !lower.contains("--noemit") {
        return true;
    }

    patterns.iter().any(|p| lower.contains(p))
}

/// Returns `true` if the shell command is primarily reading/searching.
fn is_read_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();

    // Only match if the command starts with (or pipes into) a read-like tool
    let read_starters = [
        "cat ",
        "head ",
        "tail ",
        "less ",
        "more ",
        "wc ",
        "file ",
        "stat ",
        "find ",
        "fd ",
        "rg ",
        "grep ",
        "ag ",
        "ack ",
        "ls ",
        "tree ",
        "git log",
        "git show",
        "git diff",
        "git status",
        "git blame",
    ];

    read_starters.iter().any(|p| lower.starts_with(p))
        || lower.starts_with("echo ") && !lower.contains(">>") && !lower.contains(">")
}

// =============================================================================
// Summary Generators
// =============================================================================

fn summarize_exploration(tool_name: &str, tool_input: Option<&serde_json::Value>) -> String {
    // Try to extract the file/path being explored.
    // OpenCode uses "filePath" (camelCase); other agents use "path", "file", etc.
    // For bash-based reads (ls, cat, find), the path is embedded in the command string.
    let path = tool_input
        .and_then(|v| {
            v.get("filePath")
                .or_else(|| v.get("path"))
                .or_else(|| v.get("file"))
                .or_else(|| v.get("file_path"))
                .or_else(|| v.get("glob"))
                .or_else(|| v.get("regex"))
                .or_else(|| v.get("pattern"))
        })
        .and_then(|v| v.as_str());

    // For bash tools, try to extract a meaningful target from the command string
    let bash_target = if path.is_none() {
        tool_input
            .and_then(|v| v.get("command").or_else(|| v.get("cmd")))
            .and_then(|v| v.as_str())
            .map(|cmd| {
                // Extract the last meaningful argument from common read commands
                // "ls -la /some/path" → "/some/path"
                // "cat src/index.ts" → "src/index.ts"
                // "find . -name '*.ts'" → "*.ts files"
                let cmd = cmd.trim();
                if let Some(rest) = cmd
                    .strip_prefix("cat ")
                    .or_else(|| cmd.strip_prefix("head "))
                    .or_else(|| cmd.strip_prefix("tail "))
                {
                    let target = rest.split_whitespace().last().unwrap_or(rest);
                    return shorten_explore_path(target);
                }
                if let Some(rest) = cmd.strip_prefix("ls ") {
                    let target = rest
                        .split_whitespace()
                        .rfind(|s| !s.starts_with('-'))
                        .unwrap_or(".");
                    return format!("directory {}", shorten_explore_path(target));
                }
                // For other commands, use the description if available
                String::new()
            })
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    // Also check for a human-readable description (from enriched after-tool payload)
    let description = tool_input
        .and_then(|v| v.get("description"))
        .and_then(|v| v.as_str());

    // Build the summary with the best available information
    match (path, bash_target.as_deref(), description) {
        (Some(p), _, _) => {
            let display = shorten_explore_path(p);
            let verb = match tool_name.to_lowercase().as_str() {
                "grep" | "rg" | "ripgrep" => "Search",
                "find_path" | "find" | "glob" => "Find",
                "list_directory" | "listdir" | "ls" | "tree" => "List",
                "thinking" | "think" => "Reasoning about",
                "read" | "read_file" => "Examine",
                _ => "Examine",
            };
            format!("{} {}", verb, display)
        }
        (None, Some(target), _) => {
            format!("Examine {}", target)
        }
        (None, None, Some(desc)) => truncate_for_summary(desc, 500),
        (None, None, None) => {
            let verb = match tool_name.to_lowercase().as_str() {
                "grep" | "rg" | "ripgrep" | "search" => "Search codebase",
                "find_path" | "find" | "glob" => "Find files",
                "list_directory" | "listdir" | "ls" | "tree" => "List directory",
                "thinking" | "think" => "Internal reasoning",
                "fetch" => "Fetch URL",
                "read" | "read_file" => "Examine file",
                "bash" => "Explore",
                _ => "Explore",
            };
            verb.to_string()
        }
    }
}

/// Shorten a file path for exploration summaries.
///
/// Prefers project-relative paths: `/Users/lee/Projects/hello-world/src/index.ts` → `src/index.ts`
fn shorten_explore_path(full_path: &str) -> String {
    let full_path = full_path.trim().trim_matches('"').trim_matches('\'');
    if full_path == "." || full_path == "./" {
        return "project root".to_string();
    }

    let parts: Vec<&str> = full_path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return parts.join("/");
    }

    // Look for common project markers to find the project-relative path
    let markers = ["src", "lib", "app", "test", "tests", "dist", "pkg", "cmd"];
    for (i, part) in parts.iter().enumerate() {
        if markers.contains(&part.to_lowercase().as_str()) {
            return parts[i..].join("/");
        }
    }

    // Fall back to last 2 segments
    parts[parts.len() - 2..].join("/")
}

fn summarize_commitment(tool_name: &str, tool_input: Option<&serde_json::Value>) -> String {
    let path = tool_input
        .and_then(|v| {
            v.get("path")
                .or_else(|| v.get("file"))
                .or_else(|| v.get("file_path"))
                .or_else(|| v.get("filePath"))
        })
        .and_then(|v| v.as_str());

    let verb = match tool_name.to_lowercase().as_str() {
        "create" | "create_file" | "createfile" | "write" | "write_file" | "writefile"
        | "write_to_file" => "Create",
        "delete_file" | "remove_file" => "Delete",
        "mv" | "move" | "rename" | "move_path" => "Move",
        "copy" | "copy_path" => "Copy",
        _ => "Edit",
    };

    match path {
        Some(p) => format!("{} {}", verb, p),
        None => format!("{} file", verb),
    }
}

fn summarize_verification(
    tool_input: Option<&serde_json::Value>,
    tool_output: Option<&str>,
) -> String {
    let cmd = extract_command(tool_input);
    let cmd = cmd.trim();

    let passed = tool_output.and_then(|o| {
        let lower = o.to_lowercase();
        // Heuristic: look for common pass/fail signals.
        // Order matters: "0 failed" is a PASS signal, so check it before
        // the generic "fail" pattern.
        if lower.contains("0 failed")
            || lower.contains("test result: ok")
            || lower.contains("tests passed")
        {
            Some(true)
        } else if lower.contains("fail") || lower.contains("failed") || lower.contains("error") {
            Some(false)
        } else if lower.contains("pass") || lower.contains("ok") || lower.contains("success") {
            Some(true)
        } else {
            None
        }
    });

    let result_suffix = match passed {
        Some(true) => " (passed)",
        Some(false) => " (failed)",
        None => "",
    };

    if cmd.is_empty() {
        format!("Run verification{}", result_suffix)
    } else {
        format!("{}{}", truncate_for_summary(cmd, 300), result_suffix)
    }
}

fn summarize_execution(tool_name: &str, tool_input: Option<&serde_json::Value>) -> String {
    // For shell commands, show the command
    let normalized = tool_name.to_lowercase();
    if matches!(
        normalized.as_str(),
        "bash" | "terminal" | "shell" | "command" | "exec" | "run"
    ) {
        let cmd = extract_command(tool_input);
        let cmd = cmd.trim();
        if !cmd.is_empty() {
            return truncate_for_summary(cmd, 300);
        }
    }

    format!("Execute {}", tool_name)
}

/// Truncate a string for summary display, adding "..." if truncated.
fn truncate_for_summary(s: &str, max_len: usize) -> String {
    // Take first line only
    let first_line = s.lines().next().unwrap_or(s);
    let trimmed = first_line.trim();

    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(max_len.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- classify_tool_call: Read-family tools ----

    #[test]
    fn test_classify_read() {
        assert_eq!(
            classify_tool_call("read", None, None, None),
            NodeKind::Exploration
        );
    }

    #[test]
    fn test_classify_read_file() {
        assert_eq!(
            classify_tool_call("read_file", None, None, None),
            NodeKind::Exploration
        );
    }

    #[test]
    fn test_classify_grep() {
        assert_eq!(
            classify_tool_call("grep", None, None, None),
            NodeKind::Exploration
        );
    }

    #[test]
    fn test_classify_glob() {
        assert_eq!(
            classify_tool_call("glob", None, None, None),
            NodeKind::Exploration
        );
    }

    #[test]
    fn test_classify_find_path() {
        assert_eq!(
            classify_tool_call("find_path", None, None, None),
            NodeKind::Exploration
        );
    }

    #[test]
    fn test_classify_list_directory() {
        assert_eq!(
            classify_tool_call("list_directory", None, None, None),
            NodeKind::Exploration
        );
    }

    #[test]
    fn test_classify_thinking() {
        assert_eq!(
            classify_tool_call("thinking", None, None, None),
            NodeKind::Exploration
        );
    }

    #[test]
    fn test_classify_fetch() {
        assert_eq!(
            classify_tool_call("fetch", None, None, None),
            NodeKind::Exploration
        );
    }

    #[test]
    fn test_classify_diagnostics() {
        assert_eq!(
            classify_tool_call("diagnostics", None, None, None),
            NodeKind::Exploration
        );
    }

    // ---- classify_tool_call: Write-family tools ----

    #[test]
    fn test_classify_edit() {
        assert_eq!(
            classify_tool_call("edit", None, None, None),
            NodeKind::Commitment
        );
    }

    #[test]
    fn test_classify_edit_file() {
        assert_eq!(
            classify_tool_call("edit_file", None, None, None),
            NodeKind::Commitment
        );
    }

    #[test]
    fn test_classify_write() {
        assert_eq!(
            classify_tool_call("write", None, None, None),
            NodeKind::Commitment
        );
    }

    #[test]
    fn test_classify_multiedit() {
        assert_eq!(
            classify_tool_call("multiedit", None, None, None),
            NodeKind::Commitment
        );
    }

    #[test]
    fn test_classify_patch() {
        assert_eq!(
            classify_tool_call("patch", None, None, None),
            NodeKind::Commitment
        );
    }

    #[test]
    fn test_classify_create() {
        assert_eq!(
            classify_tool_call("create", None, None, None),
            NodeKind::Commitment
        );
    }

    #[test]
    fn test_classify_delete_file() {
        assert_eq!(
            classify_tool_call("delete_file", None, None, None),
            NodeKind::Commitment
        );
    }

    #[test]
    fn test_classify_move_path() {
        assert_eq!(
            classify_tool_call("move_path", None, None, None),
            NodeKind::Commitment
        );
    }

    #[test]
    fn test_classify_save_file() {
        assert_eq!(
            classify_tool_call("save_file", None, None, None),
            NodeKind::Commitment
        );
    }

    // ---- classify_tool_call: Error status overrides everything ----

    #[test]
    fn test_classify_error_status_overrides_read() {
        assert_eq!(
            classify_tool_call("read", None, None, Some("error")),
            NodeKind::Error,
        );
    }

    #[test]
    fn test_classify_error_status_overrides_edit() {
        assert_eq!(
            classify_tool_call("edit", None, None, Some("error")),
            NodeKind::Error,
        );
    }

    #[test]
    fn test_classify_error_status_overrides_bash() {
        assert_eq!(
            classify_tool_call("bash", None, None, Some("error")),
            NodeKind::Error,
        );
    }

    #[test]
    fn test_classify_completed_status_does_not_override() {
        assert_eq!(
            classify_tool_call("read", None, None, Some("completed")),
            NodeKind::Exploration,
        );
    }

    // ---- classify_tool_call: Shell command sub-classification ----

    #[test]
    fn test_classify_bash_cargo_test() {
        let input = serde_json::json!({"command": "cargo test --lib"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_npm_test() {
        let input = serde_json::json!({"command": "npm test"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_bun_test() {
        let input = serde_json::json!({"command": "bun test"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_pytest() {
        let input = serde_json::json!({"command": "pytest tests/"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_jest() {
        let input = serde_json::json!({"command": "npx jest --coverage"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_vitest() {
        let input = serde_json::json!({"command": "npx vitest run"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_go_test() {
        let input = serde_json::json!({"command": "go test ./..."});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_eslint() {
        let input = serde_json::json!({"command": "npx eslint src/"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_cargo_clippy() {
        let input = serde_json::json!({"command": "cargo clippy -- -D warnings"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_tsc_noemit() {
        let input = serde_json::json!({"command": "tsc --noEmit"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_cargo_build() {
        let input = serde_json::json!({"command": "cargo build --release"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_cargo_check() {
        let input = serde_json::json!({"command": "cargo check"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_tsc_build() {
        let input = serde_json::json!({"command": "tsc"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_npm_install_is_execution() {
        let input = serde_json::json!({"command": "npm install express"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Execution,
        );
    }

    #[test]
    fn test_classify_bash_curl_is_execution() {
        let input = serde_json::json!({"command": "curl -X POST https://api.example.com"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Execution,
        );
    }

    #[test]
    fn test_classify_bash_mkdir_is_execution() {
        let input = serde_json::json!({"command": "mkdir -p src/new_module"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Execution,
        );
    }

    #[test]
    fn test_classify_bash_cat_is_exploration() {
        let input = serde_json::json!({"command": "cat src/main.rs"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Exploration,
        );
    }

    #[test]
    fn test_classify_bash_git_log_is_exploration() {
        let input = serde_json::json!({"command": "git log --oneline -10"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Exploration,
        );
    }

    #[test]
    fn test_classify_bash_git_diff_is_exploration() {
        let input = serde_json::json!({"command": "git diff HEAD~1"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Exploration,
        );
    }

    #[test]
    fn test_classify_bash_find_is_exploration() {
        let input = serde_json::json!({"command": "find . -name '*.rs'"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Exploration,
        );
    }

    #[test]
    fn test_classify_bash_no_command_is_execution() {
        let input = serde_json::json!({});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Execution,
        );
    }

    #[test]
    fn test_classify_bash_empty_command_is_execution() {
        let input = serde_json::json!({"command": ""});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Execution,
        );
    }

    #[test]
    fn test_classify_terminal_same_as_bash() {
        let input = serde_json::json!({"command": "cargo test"});
        assert_eq!(
            classify_tool_call("terminal", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    // ---- classify_tool_call: Case insensitivity ----

    #[test]
    fn test_classify_case_insensitive() {
        assert_eq!(
            classify_tool_call("Read", None, None, None),
            NodeKind::Exploration
        );
        assert_eq!(
            classify_tool_call("EDIT", None, None, None),
            NodeKind::Commitment
        );
        assert_eq!(
            classify_tool_call("Bash", None, None, None),
            NodeKind::Execution
        );
    }

    // ---- classify_tool_call: Unknown tools ----

    #[test]
    fn test_classify_unknown_tool_is_execution() {
        assert_eq!(
            classify_tool_call("my_custom_tool", None, None, None),
            NodeKind::Execution,
        );
    }

    #[test]
    fn test_classify_task_is_execution() {
        assert_eq!(
            classify_tool_call("task", None, None, None),
            NodeKind::Execution
        );
    }

    // ---- classify_tool_call: Input field extraction ----

    #[test]
    fn test_classify_bash_cmd_field() {
        let input = serde_json::json!({"cmd": "cargo test"});
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    #[test]
    fn test_classify_bash_bare_string_input() {
        let input = serde_json::json!("cargo test");
        assert_eq!(
            classify_tool_call("bash", Some(&input), None, None),
            NodeKind::Verification,
        );
    }

    // ---- summarize_tool_call ----

    #[test]
    fn test_summarize_exploration_with_path() {
        let input = serde_json::json!({"path": "src/auth/login.rs"});
        let summary = summarize_tool_call("read", NodeKind::Exploration, Some(&input), None, None);
        assert_eq!(summary, "Examine src/auth/login.rs");
    }

    #[test]
    fn test_summarize_exploration_grep_with_regex() {
        let input = serde_json::json!({"regex": "fn validate_token"});
        let summary = summarize_tool_call("grep", NodeKind::Exploration, Some(&input), None, None);
        assert_eq!(summary, "Search fn validate_token");
    }

    #[test]
    fn test_summarize_exploration_no_path() {
        let summary = summarize_tool_call("read", NodeKind::Exploration, None, None, None);
        assert_eq!(summary, "Examine file");
    }

    #[test]
    fn test_summarize_exploration_list_directory() {
        let input = serde_json::json!({"path": "src/auth/"});
        let summary = summarize_tool_call(
            "list_directory",
            NodeKind::Exploration,
            Some(&input),
            None,
            None,
        );
        assert_eq!(summary, "List src/auth");
    }

    #[test]
    fn test_summarize_commitment_edit() {
        let input = serde_json::json!({"path": "src/auth/login.rs"});
        let summary = summarize_tool_call("edit", NodeKind::Commitment, Some(&input), None, None);
        assert_eq!(summary, "Edit src/auth/login.rs");
    }

    #[test]
    fn test_summarize_commitment_create() {
        let input = serde_json::json!({"path": "src/new_module.rs"});
        let summary = summarize_tool_call("create", NodeKind::Commitment, Some(&input), None, None);
        assert_eq!(summary, "Create src/new_module.rs");
    }

    #[test]
    fn test_summarize_commitment_no_path() {
        let summary = summarize_tool_call("edit", NodeKind::Commitment, None, None, None);
        assert_eq!(summary, "Edit file");
    }

    #[test]
    fn test_summarize_verification_with_pass() {
        let input = serde_json::json!({"command": "cargo test --lib"});
        let summary = summarize_tool_call(
            "bash",
            NodeKind::Verification,
            Some(&input),
            Some("test result: ok. 12 passed; 0 failed"),
            None,
        );
        assert_eq!(summary, "cargo test --lib (passed)");
    }

    #[test]
    fn test_summarize_verification_with_fail() {
        let input = serde_json::json!({"command": "cargo test"});
        let summary = summarize_tool_call(
            "bash",
            NodeKind::Verification,
            Some(&input),
            Some("test auth::tests::test_login ... FAILED"),
            None,
        );
        assert_eq!(summary, "cargo test (failed)");
    }

    #[test]
    fn test_summarize_verification_no_result() {
        let input = serde_json::json!({"command": "cargo test"});
        let summary = summarize_tool_call("bash", NodeKind::Verification, Some(&input), None, None);
        assert_eq!(summary, "cargo test");
    }

    #[test]
    fn test_summarize_execution() {
        let input = serde_json::json!({"command": "npm install express"});
        let summary = summarize_tool_call("bash", NodeKind::Execution, Some(&input), None, None);
        assert_eq!(summary, "npm install express");
    }

    #[test]
    fn test_summarize_error() {
        let summary = summarize_tool_call(
            "edit",
            NodeKind::Error,
            None,
            Some("File not found: src/missing.rs"),
            Some("error"),
        );
        assert_eq!(summary, "edit failed: File not found: src/missing.rs");
    }

    #[test]
    fn test_summarize_error_no_output() {
        let summary = summarize_tool_call("edit", NodeKind::Error, None, None, Some("error"));
        assert_eq!(summary, "edit failed");
    }

    // ---- truncate_for_summary ----

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate_for_summary("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_long_string() {
        let long = "a".repeat(100);
        let result = truncate_for_summary(&long, 20);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 20);
    }

    #[test]
    fn test_truncate_multiline_uses_first_line() {
        let multi = "first line\nsecond line\nthird line";
        assert_eq!(truncate_for_summary(multi, 100), "first line");
    }

    #[test]
    fn test_truncate_trims_whitespace() {
        assert_eq!(truncate_for_summary("  hello  ", 100), "hello");
    }

    // ---- extract_command ----

    #[test]
    fn test_extract_command_from_command_field() {
        let input = serde_json::json!({"command": "cargo test"});
        assert_eq!(extract_command(Some(&input)), "cargo test");
    }

    #[test]
    fn test_extract_command_from_cmd_field() {
        let input = serde_json::json!({"cmd": "npm install"});
        assert_eq!(extract_command(Some(&input)), "npm install");
    }

    #[test]
    fn test_extract_command_from_bare_string() {
        let input = serde_json::json!("cargo build");
        assert_eq!(extract_command(Some(&input)), "cargo build");
    }

    #[test]
    fn test_extract_command_none_input() {
        assert_eq!(extract_command(None), "");
    }

    #[test]
    fn test_extract_command_no_known_fields() {
        let input = serde_json::json!({"foo": "bar"});
        assert_eq!(extract_command(Some(&input)), "");
    }

    // ---- is_test_command ----

    #[test]
    fn test_is_test_cargo_test() {
        assert!(is_test_command("cargo test --lib"));
    }

    #[test]
    fn test_is_test_cargo_nextest() {
        assert!(is_test_command("cargo nextest run"));
    }

    #[test]
    fn test_is_test_mixed_case() {
        assert!(is_test_command("CARGO TEST"));
    }

    #[test]
    fn test_is_not_test_cargo_build() {
        assert!(!is_test_command("cargo build"));
    }

    // ---- is_lint_command ----

    #[test]
    fn test_is_lint_eslint() {
        assert!(is_lint_command("npx eslint src/"));
    }

    #[test]
    fn test_is_lint_clippy() {
        assert!(is_lint_command("cargo clippy -- -D warnings"));
    }

    #[test]
    fn test_is_not_lint_cargo_test() {
        assert!(!is_lint_command("cargo test"));
    }

    // ---- is_build_command ----

    #[test]
    fn test_is_build_cargo_build() {
        assert!(is_build_command("cargo build --release"));
    }

    #[test]
    fn test_is_build_tsc() {
        assert!(is_build_command("tsc"));
    }

    #[test]
    fn test_is_not_build_tsc_noemit() {
        // tsc --noEmit is a typecheck, not a build
        assert!(!is_build_command("tsc --noEmit"));
    }

    #[test]
    fn test_is_build_npm_run_build() {
        assert!(is_build_command("npm run build"));
    }

    #[test]
    fn test_is_not_build_npm_install() {
        assert!(!is_build_command("npm install"));
    }

    // ---- is_read_command ----

    #[test]
    fn test_is_read_cat() {
        assert!(is_read_command("cat src/main.rs"));
    }

    #[test]
    fn test_is_read_git_log() {
        assert!(is_read_command("git log --oneline"));
    }

    #[test]
    fn test_is_read_find() {
        assert!(is_read_command("find . -name '*.rs'"));
    }

    #[test]
    fn test_is_not_read_npm_install() {
        assert!(!is_read_command("npm install"));
    }

    #[test]
    fn test_is_not_read_echo_redirect() {
        // echo with redirect is a write, not a read
        assert!(!is_read_command("echo 'hello' >> file.txt"));
    }
}
