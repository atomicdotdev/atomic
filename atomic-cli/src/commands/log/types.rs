use super::*;

/// Output format for the log command.
///
/// This enum defines the available output formats for displaying history.
/// Each format is optimized for different use cases: human reading,
/// scripting, or compact viewing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum LogFormat {
    /// Full detailed format with all information.
    ///
    /// Shows hash, author, date, and full message including description.
    /// This is similar to `git log` default output.
    #[default]
    Default,

    /// Short format showing hash and first line of message.
    ///
    /// Useful for quick scanning of history.
    Short,

    /// Single-line format with hash, date, author, and message.
    ///
    /// Very compact, suitable for terminal viewing of long histories.
    Oneline,

    /// JSON format for machine parsing.
    ///
    /// Outputs a JSON array of change objects with full metadata.
    Json,
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::Default => write!(f, "default"),
            LogFormat::Short => write!(f, "short"),
            LogFormat::Oneline => write!(f, "oneline"),
            LogFormat::Json => write!(f, "json"),
        }
    }
}

impl std::str::FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "default" | "full" => Ok(LogFormat::Default),
            "short" => Ok(LogFormat::Short),
            "oneline" | "one" | "1" => Ok(LogFormat::Oneline),
            "json" => Ok(LogFormat::Json),
            _ => Err(format!(
                "Invalid format '{}'. Expected: default, short, oneline, json",
                s
            )),
        }
    }
}

// Log Output Configuration

// JSON Output Types

/// JSON representation of an author.
///
/// Used for JSON output format serialization.
#[derive(Debug, Clone, Serialize)]
pub struct JsonAuthor {
    /// The author's name.
    pub name: String,
    /// The author's email (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl From<&Author> for JsonAuthor {
    fn from(author: &Author) -> Self {
        Self {
            name: author.name.clone(),
            email: author.email.clone(),
        }
    }
}

/// JSON representation of a history entry.
///
/// This struct provides a complete JSON serialization of a history entry,
/// suitable for machine parsing and scripting.
#[derive(Debug, Clone, Serialize)]
pub struct JsonLogEntry {
    /// The sequence number in the stack.
    pub sequence: u64,

    /// The change hash (full Base32 encoding).
    pub hash: String,

    /// The Merkle state after this change.
    pub state: String,

    /// The change message (if header loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// The change description (if header loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The authors (if header loaded).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<JsonAuthor>,

    /// The timestamp (ISO 8601 format).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// Whether this change is tagged.
    pub is_tagged: bool,
}

impl JsonLogEntry {
    /// Create a JSON entry from a history entry.
    ///
    /// # Arguments
    ///
    /// * `entry` - The history entry to convert
    ///
    /// # Returns
    ///
    /// A `JsonLogEntry` with all available information.
    pub fn from_entry(entry: &HistoryEntry) -> Self {
        Self {
            sequence: entry.sequence,
            hash: entry.hash.to_base32(),
            state: entry.state.to_base32(),
            message: entry.message().map(String::from),
            description: entry.description().map(String::from),
            authors: entry
                .authors()
                .map(|authors| authors.iter().map(JsonAuthor::from).collect())
                .unwrap_or_default(),
            timestamp: entry.timestamp().map(|ts| ts.to_rfc3339()),
            is_tagged: entry.is_tagged,
        }
    }
}
