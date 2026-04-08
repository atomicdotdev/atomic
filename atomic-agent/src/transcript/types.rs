use std::fmt;

use serde::{Deserialize, Serialize};

// Condensed Transcript

/// The type of a transcript entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    /// A user prompt.
    User,
    /// An assistant (model) response.
    Assistant,
    /// A tool invocation.
    Tool,
}

impl fmt::Display for EntryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryType::User => write!(f, "User"),
            EntryType::Assistant => write!(f, "Assistant"),
            EntryType::Tool => write!(f, "Tool"),
        }
    }
}

/// A single entry in the condensed transcript.
///
/// The raw JSONL/JSON transcript from the agent is parsed into these
/// structured entries, filtering out noise (file contents from Read tools,
/// verbose bash output, skill injections).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CondensedEntry {
    /// The type of entry.
    pub entry_type: EntryType,

    /// Text content for user/assistant entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Tool name for tool entries (e.g., "Edit", "Bash", "Read").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,

    /// Tool detail — a description, file path, command, or URL.
    ///
    /// For verbose tools (Read, WebFetch, Skill), this shows only the
    /// path/URL, not the full content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_detail: Option<String>,
}

impl CondensedEntry {
    /// Create a user prompt entry.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            entry_type: EntryType::User,
            content: Some(content.into()),
            tool_name: None,
            tool_detail: None,
        }
    }

    /// Create an assistant response entry.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            entry_type: EntryType::Assistant,
            content: Some(content.into()),
            tool_name: None,
            tool_detail: None,
        }
    }

    /// Create a tool invocation entry.
    pub fn tool(name: impl Into<String>, detail: Option<impl Into<String>>) -> Self {
        Self {
            entry_type: EntryType::Tool,
            content: None,
            tool_name: Some(name.into()),
            tool_detail: detail.map(Into::into),
        }
    }

    /// Returns true if this is a user entry.
    pub fn is_user(&self) -> bool {
        self.entry_type == EntryType::User
    }

    /// Returns true if this is an assistant entry.
    pub fn is_assistant(&self) -> bool {
        self.entry_type == EntryType::Assistant
    }

    /// Returns true if this is a tool entry.
    pub fn is_tool(&self) -> bool {
        self.entry_type == EntryType::Tool
    }
}

impl fmt::Display for CondensedEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.entry_type {
            EntryType::User => {
                write!(f, "[User] {}", self.content.as_deref().unwrap_or(""))
            }
            EntryType::Assistant => {
                write!(f, "[Assistant] {}", self.content.as_deref().unwrap_or(""))
            }
            EntryType::Tool => {
                let name = self.tool_name.as_deref().unwrap_or("Unknown");
                match &self.tool_detail {
                    Some(detail) => write!(f, "[Tool] {}: {}", name, detail),
                    None => write!(f, "[Tool] {}", name),
                }
            }
        }
    }
}

// Tool Usage Summary

/// Aggregated tool usage for a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUseSummary {
    /// Tool name (e.g., "Edit", "Bash", "Read").
    pub tool_name: String,

    /// Number of times the tool was invoked in this turn.
    pub invocation_count: u32,

    /// Files affected by this tool (deduplicated).
    #[serde(default)]
    pub files_affected: Vec<String>,
}

impl ToolUseSummary {
    /// Create a new tool usage summary.
    pub fn new(name: impl Into<String>, count: u32, files: Vec<String>) -> Self {
        Self {
            tool_name: name.into(),
            invocation_count: count,
            files_affected: files,
        }
    }
}

impl fmt::Display for ToolUseSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (×{})", self.tool_name, self.invocation_count)?;
        if !self.files_affected.is_empty() {
            write!(f, " → {}", self.files_affected.join(", "))?;
        }
        Ok(())
    }
}

// Claude Code JSONL Types (internal)

/// A single line in a Claude Code JSONL transcript.
#[derive(Debug, Deserialize)]
pub(crate) struct TranscriptLine {
    pub r#type: String,
    #[allow(dead_code)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub message: serde_json::Value,
}

/// An assistant message containing content blocks.
#[derive(Debug, Deserialize)]
pub(crate) struct AssistantMessage {
    pub content: Vec<ContentBlock>,
}

/// A content block within an assistant message.
#[derive(Debug, Deserialize)]
pub(crate) struct ContentBlock {
    pub r#type: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: serde_json::Value,
}

/// Tool input fields (best-effort extraction).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ToolInput {
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub notebook_path: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}
