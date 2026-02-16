use super::*;

/// Output format for the change command.
///
/// This enum defines the available output formats for displaying change details.
/// Each format is optimized for different use cases: human reading, quick scanning,
/// or machine parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum ChangeFormat {
    /// Full detailed format with all information.
    ///
    /// Shows hash, author, date, message, description, dependencies, and hunks summary.
    #[default]
    Default,

    /// Short single-line format.
    ///
    /// Shows hash, date, author, and message first line.
    Short,

    /// JSON format for machine parsing.
    ///
    /// Outputs a JSON object with all change metadata.
    Json,
}

impl std::fmt::Display for ChangeFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeFormat::Default => write!(f, "default"),
            ChangeFormat::Short => write!(f, "short"),
            ChangeFormat::Json => write!(f, "json"),
        }
    }
}

impl std::str::FromStr for ChangeFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "default" | "full" => Ok(ChangeFormat::Default),
            "short" => Ok(ChangeFormat::Short),
            "json" => Ok(ChangeFormat::Json),
            _ => Err(format!(
                "Invalid format '{}'. Expected: default, short, json",
                s
            )),
        }
    }
}

// Change Identifier

/// Parsed change identifier.
///
/// A change can be identified by its hash (full or prefix) or by its
/// sequence number in a stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeIdentifier {
    /// Full 52-character Base32 hash.
    FullHash(Hash),

    /// Hash prefix (4-51 characters).
    HashPrefix(String),

    /// Sequence number in the stack's history.
    Sequence(u64),

    /// No identifier - use most recent change.
    Latest,
}

impl ChangeIdentifier {
    /// Parse a change identifier from a string.
    ///
    /// # Arguments
    ///
    /// * `s` - The identifier string
    ///
    /// # Returns
    ///
    /// The parsed `ChangeIdentifier`.
    ///
    /// # Parsing Rules
    ///
    /// - If the string is empty or None, returns `Latest`
    /// - If the string starts with `#`, parses as sequence number
    /// - If the string is purely numeric, parses as sequence number
    /// - If the string is exactly 52 base32 characters, parses as `FullHash`
    /// - Otherwise, parses as `HashPrefix`
    pub fn parse(s: Option<&str>) -> Result<Self, String> {
        let s = match s {
            None => return Ok(ChangeIdentifier::Latest),
            Some(s) if s.is_empty() => return Ok(ChangeIdentifier::Latest),
            Some(s) => s.trim(),
        };

        // Check for sequence number with # prefix
        if let Some(num_str) = s.strip_prefix('#') {
            return num_str
                .parse::<u64>()
                .map(ChangeIdentifier::Sequence)
                .map_err(|_| format!("Invalid sequence number: {}", num_str));
        }

        // Check for pure numeric (sequence number)
        if s.chars().all(|c| c.is_ascii_digit()) {
            return s
                .parse::<u64>()
                .map(ChangeIdentifier::Sequence)
                .map_err(|_| format!("Invalid sequence number: {}", s));
        }

        // Check for valid Base32 characters
        let upper = s.to_uppercase();
        if !upper
            .chars()
            .all(|c| "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".contains(c))
        {
            return Err(format!(
                "Invalid hash characters in '{}'. Hashes use Base32 encoding (A-Z, 2-7).",
                s
            ));
        }

        // Full hash (52 characters)
        if upper.len() == 52 {
            return Hash::from_base32(upper.as_bytes())
                .map(ChangeIdentifier::FullHash)
                .ok_or_else(|| format!("Invalid Base32 hash: {}", s));
        }

        // Hash prefix (4-51 characters)
        if upper.len() >= 4 {
            Ok(ChangeIdentifier::HashPrefix(upper))
        } else {
            Err(format!(
                "Hash prefix too short: '{}'. Minimum 4 characters required.",
                s
            ))
        }
    }
}

// JSON Output Types

/// JSON representation of an author.
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

/// JSON representation of a graph_op summary.
#[derive(Debug, Clone, Serialize)]
pub struct JsonHunkSummary {
    /// Type of graph_op (FileAdd, FileDel, FileMove, Edit, etc.)
    pub hunk_type: String,
    /// Path affected (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// JSON representation of a change.
#[derive(Debug, Clone, Serialize)]
pub struct JsonChange {
    /// The full Base32 hash.
    pub hash: String,

    /// The change message.
    pub message: String,

    /// The description (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The authors.
    pub authors: Vec<JsonAuthor>,

    /// The timestamp (ISO 8601 format).
    pub timestamp: String,

    /// Dependencies (as Base32 hashes).
    pub dependencies: Vec<String>,

    /// GraphOp summaries.
    pub hunks: Vec<JsonHunkSummary>,

    /// Whether this change has AI provenance information.
    pub has_provenance: bool,

    /// AI provenance details (if available and requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<JsonProvenance>,

    /// Sequence number in the current stack (if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

impl JsonChange {
    /// Create a JSON representation from a change and its hash.
    ///
    /// # Arguments
    ///
    /// * `change` - The change to represent
    /// * `hash` - The change's hash
    /// * `sequence` - Optional sequence number in the stack
    pub fn from_change(change: &Change, hash: &Hash, sequence: Option<u64>) -> Self {
        Self {
            hash: hash.to_base32(),
            message: change.hashed.header.message.clone(),
            description: change.hashed.header.description.clone(),
            authors: change
                .hashed
                .header
                .authors
                .iter()
                .map(JsonAuthor::from)
                .collect(),
            timestamp: change.hashed.header.timestamp.to_rfc3339(),
            dependencies: change
                .hashed
                .dependencies
                .iter()
                .map(|h| h.to_base32())
                .collect(),
            hunks: change
                .hashed
                .hunks
                .iter()
                .map(|h| hunk_to_summary(h))
                .collect(),
            has_provenance: change.has_provenance(),
            provenance: None,
            sequence,
        }
    }

    /// Create from a Change with provenance details.
    pub fn from_change_with_provenance(
        change: &Change,
        hash: &Hash,
        sequence: Option<u64>,
    ) -> Self {
        let mut json = Self::from_change(change, hash, sequence);
        if let Some(prov) = change.hashed.provenance.first() {
            json.provenance = Some(JsonProvenance::from(prov));
        }
        json
    }
}

/// JSON representation of AI provenance.
#[derive(Debug, Clone, Serialize)]
pub struct JsonProvenance {
    /// AI vendor/provider.
    pub vendor: String,
    /// Model identifier.
    pub model: String,
    /// Model version (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
    /// Tool used to interact with the AI.
    pub tool: String,
    /// Type of AI contribution.
    pub suggestion_type: String,
    /// Token usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<JsonTokenUsage>,
    /// Cost information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<JsonCost>,
    /// Temperature setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Request timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// Request ID from provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Session ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// JSON representation of token usage.
#[derive(Debug, Clone, Serialize)]
pub struct JsonTokenUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// JSON representation of cost.
#[derive(Debug, Clone, Serialize)]
pub struct JsonCost {
    pub amount_micros: u64,
    pub currency: String,
}

impl From<&Provenance> for JsonProvenance {
    fn from(prov: &Provenance) -> Self {
        let tokens = if !prov.tokens.is_empty() {
            Some(JsonTokenUsage {
                input: Some(prov.tokens.input_tokens),
                output: Some(prov.tokens.output_tokens),
                total: Some(prov.tokens.total_tokens),
            })
        } else {
            None
        };

        let cost = if !prov.cost.is_zero() {
            Some(JsonCost {
                amount_micros: prov.cost.micro_usd,
                currency: "USD".to_string(),
            })
        } else {
            None
        };

        Self {
            vendor: format!("{:?}", prov.vendor),
            model: prov.model.clone(),
            model_version: prov.model_version.clone(),
            tool: format!("{:?}", prov.tool),
            suggestion_type: format!("{:?}", prov.suggestion_type),
            tokens,
            cost,
            temperature: prov.temperature.map(|t| t as f64 / 1000.0),
            timestamp: prov.timestamp,
            request_id: prov.request_id.clone(),
            session_id: prov.session_id.clone(),
        }
    }
}

/// Count the total atoms (vertices and edges) in hunks.
pub(super) fn count_atoms<H>(hunks: &[GraphOp<H>]) -> (usize, usize) {
    let mut vertices = 0;
    let mut edges = 0;

    for graph_op in hunks {
        match graph_op {
            GraphOp::FileAdd { contents, .. } => {
                // FileAdd creates: name span, inode span, and optionally content span
                vertices += 2;
                if contents.is_some() {
                    vertices += 1;
                }
            }
            GraphOp::FileDel { contents, .. } => {
                // FileDel creates edge maps to mark as deleted
                edges += 1;
                if contents.is_some() {
                    edges += 1;
                }
            }
            GraphOp::FileUndel { contents, .. } => {
                // FileUndel creates edge maps to resurrect
                edges += 1;
                if contents.is_some() {
                    edges += 1;
                }
            }
            GraphOp::FileMove { .. } => {
                // FileMove: delete old name edge, add new name span
                vertices += 1;
                edges += 1;
            }
            GraphOp::DirAdd { .. } => {
                // DirAdd creates: name span, inode span (no content)
                vertices += 2;
            }
            GraphOp::DirDel { .. } => {
                // DirDel creates edge maps to mark as deleted
                edges += 1;
            }
            GraphOp::DirUndel { .. } => {
                // DirUndel creates edge maps to resurrect
                edges += 1;
            }
            GraphOp::Edit { .. } => {
                // Edit creates a new content span
                vertices += 1;
            }
            GraphOp::Replacement { .. } => {
                // Replacement: new span + edge map for deletion
                vertices += 1;
                edges += 1;
            }
            GraphOp::SolveNameConflict { .. } | GraphOp::UnsolveNameConflict { .. } => {
                edges += 1;
            }
            GraphOp::SolveOrderConflict { .. } | GraphOp::UnsolveOrderConflict { .. } => {
                edges += 1;
            }
            GraphOp::ResurrectZombies { .. } => {
                edges += 1;
            }
            GraphOp::AddRoot { .. } => {
                vertices += 1;
            }
            GraphOp::DelRoot { .. } => {
                edges += 1;
            }
        }
    }

    (vertices, edges)
}

/// Get a short description of the atoms in a graph_op.
pub(super) fn hunk_atom_info<H>(graph_op: &GraphOp<H>) -> String {
    match graph_op {
        GraphOp::FileAdd { contents, .. } => {
            if contents.is_some() {
                "(+3 vertices: name, inode, content)".to_string()
            } else {
                "(+2 vertices: name, inode)".to_string()
            }
        }
        GraphOp::FileDel { .. } => "(~edges: mark deleted)".to_string(),
        GraphOp::FileUndel { .. } => "(~edges: resurrect)".to_string(),
        GraphOp::FileMove { .. } => "(+1 span, ~1 edge: rename)".to_string(),
        GraphOp::DirAdd { .. } => "(+2 vertices: name, inode)".to_string(),
        GraphOp::DirDel { .. } => "(~edges: mark deleted)".to_string(),
        GraphOp::DirUndel { .. } => "(~edges: resurrect)".to_string(),
        GraphOp::Edit { .. } => "(+1 span: new content)".to_string(),
        GraphOp::Replacement { .. } => "(+1 span, ~1 edge: replace)".to_string(),
        GraphOp::SolveNameConflict { .. } => "(~edges: resolve name conflict)".to_string(),
        GraphOp::UnsolveNameConflict { .. } => "(~edges: unresolve name conflict)".to_string(),
        GraphOp::SolveOrderConflict { .. } => "(~edges: resolve order conflict)".to_string(),
        GraphOp::UnsolveOrderConflict { .. } => "(~edges: unresolve order conflict)".to_string(),
        GraphOp::ResurrectZombies { .. } => "(~edges: resurrect zombies)".to_string(),
        GraphOp::AddRoot { .. } => "(+1 span: root)".to_string(),
        GraphOp::DelRoot { .. } => "(~edges: delete root)".to_string(),
    }
}

/// Convert a graph_op to a summary for JSON output.
pub(super) fn hunk_to_summary<H>(graph_op: &GraphOp<H>) -> JsonHunkSummary {
    match graph_op {
        GraphOp::FileAdd { path, .. } => JsonHunkSummary {
            hunk_type: "FileAdd".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::FileDel { path, .. } => JsonHunkSummary {
            hunk_type: "FileDel".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::FileMove { path, .. } => JsonHunkSummary {
            hunk_type: "FileMove".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::FileUndel { path, .. } => JsonHunkSummary {
            hunk_type: "FileUndel".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::DirAdd { path, .. } => JsonHunkSummary {
            hunk_type: "DirAdd".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::DirDel { path, .. } => JsonHunkSummary {
            hunk_type: "DirDel".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::DirUndel { path, .. } => JsonHunkSummary {
            hunk_type: "DirUndel".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::Edit { local, .. } => JsonHunkSummary {
            hunk_type: "Edit".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::Replacement { local, .. } => JsonHunkSummary {
            hunk_type: "Replacement".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::SolveNameConflict { path, .. } => JsonHunkSummary {
            hunk_type: "SolveNameConflict".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::UnsolveNameConflict { path, .. } => JsonHunkSummary {
            hunk_type: "UnsolveNameConflict".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::SolveOrderConflict { local, .. } => JsonHunkSummary {
            hunk_type: "SolveOrderConflict".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::UnsolveOrderConflict { local, .. } => JsonHunkSummary {
            hunk_type: "UnsolveOrderConflict".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::ResurrectZombies { local, .. } => JsonHunkSummary {
            hunk_type: "ResurrectZombies".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::AddRoot { .. } => JsonHunkSummary {
            hunk_type: "AddRoot".to_string(),
            path: None,
        },
        GraphOp::DelRoot { .. } => JsonHunkSummary {
            hunk_type: "DelRoot".to_string(),
            path: None,
        },
    }
}
