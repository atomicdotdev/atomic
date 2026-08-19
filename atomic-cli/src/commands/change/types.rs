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
/// sequence number in a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeIdentifier {
    /// Full 52-character Base32 hash.
    FullHash(Hash),

    /// Hash prefix (4-51 characters).
    HashPrefix(String),

    /// Sequence number in the view's history.
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
            None | Some("") => return Ok(ChangeIdentifier::Latest),
            Some(s) => s.trim(),
        };

        // Check for @ syntax (latest change or relative reference)
        if s == "@" {
            return Ok(ChangeIdentifier::Latest);
        }

        // Check for @~N syntax (N changes before latest)
        if let Some(offset_str) = s.strip_prefix("@~") {
            // This would need special handling in the resolve step
            // For now, we'll treat it as a relative sequence lookup
            // which requires the view's length
            return Err(format!(
                "Relative reference '@~N' is not yet supported: {}",
                s
            ));
        }

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

    /// Check if this identifier is a hash (full or prefix).
    pub fn is_hash(&self) -> bool {
        matches!(self, Self::FullHash(_) | Self::HashPrefix(_))
    }

    /// Check if this identifier is a sequence number.
    pub fn is_sequence(&self) -> bool {
        matches!(self, Self::Sequence(_))
    }

    /// Check if this identifier is "latest" (no specific identifier).
    pub fn is_latest(&self) -> bool {
        matches!(self, Self::Latest)
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

    /// Data stored outside the content hash — the agent turn transcript
    /// (`agent_turn`) and anything else attached unhashed — when the change
    /// carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unhashed: Option<serde_json::Value>,

    /// Change ledger(s): the causal decision graph(s) explaining this change,
    /// loaded from the `.provenance` file. Empty when no provenance graph is
    /// recorded for the change. This is the machine-readable equivalent of the
    /// `=== Change Ledger ===` section in the default text output.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ledger: Vec<JsonChangeLedger>,

    /// Sequence number in the current view (if known).
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
    /// * `sequence` - Optional sequence number in the view
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
            hunks: change.hashed.hunks.iter().map(hunk_to_summary).collect(),
            has_provenance: change.has_provenance(),
            provenance: None,
            unhashed: change.unhashed.clone(),
            ledger: Vec::new(),
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

    /// Attach the change ledger(s) (causal decision graph) to this JSON change.
    ///
    /// Consumes and returns `self` so it can be chained after construction.
    pub fn with_ledger(mut self, ledger: Vec<JsonChangeLedger>) -> Self {
        self.ledger = ledger;
        self
    }
}

/// JSON representation of a change ledger (one provenance/decision graph).
///
/// Mirrors [`atomic_core::change::ProvenanceGraph`] but flattens node/edge
/// kinds to their stable snake_case labels and encodes hashes as Base32 so the
/// output is stable and self-describing for external tooling.
#[derive(Debug, Clone, Serialize)]
pub struct JsonChangeLedger {
    /// Base32 hash of the provenance graph itself.
    pub graph_hash: String,
    /// Session identifier linking graphs within the same session.
    pub session_id: String,
    /// Agent registry key (e.g. "claude-code", "opencode").
    pub agent_name: String,
    /// Human-readable agent name (e.g. "Claude Code").
    #[serde(skip_serializing_if = "String::is_empty")]
    pub agent_display_name: String,
    /// AI vendor (e.g. "anthropic", "openai").
    #[serde(skip_serializing_if = "String::is_empty")]
    pub agent_vendor: String,
    /// Graph creation timestamp (Unix epoch seconds).
    pub timestamp: i64,
    /// Schema profile identifier (e.g. "sherpa-trace/1.0.0"), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Vault work-item/intent ID governing this turn, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    /// Number of nodes in the decision graph.
    pub node_count: usize,
    /// Number of causal edges in the decision graph.
    pub edge_count: usize,
    /// Number of changes this graph explains.
    pub change_count: usize,
    /// Base32 hashes of the changes this graph explains.
    pub changes_explained: Vec<String>,
    /// Base32 hash of the previous graph in this session chain, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// The typed nodes in the decision DAG.
    pub nodes: Vec<JsonLedgerNode>,
    /// The causal edges between nodes.
    pub edges: Vec<JsonLedgerEdge>,
}

/// JSON representation of a single provenance graph node.
#[derive(Debug, Clone, Serialize)]
pub struct JsonLedgerNode {
    /// Unique node identifier within the graph.
    pub id: String,
    /// Node kind label (snake_case: "goal", "exploration", "commitment", ...).
    pub kind: String,
    /// When this activity occurred (Unix epoch milliseconds).
    pub timestamp: i64,
    /// One-line human-readable summary.
    pub summary: String,
    /// Kind-specific structured detail. Parsed to JSON when it is valid JSON,
    /// otherwise carried as a raw string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// Base32 hash of the change this node produced, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_hash: Option<String>,
    /// Tool name that produced this node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Tool call ID for correlating pre/post pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Duration of the tool call in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Whether the classifier consolidated this node.
    pub classified: bool,
    /// Classifier confidence (0.0–1.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// IDs of raw tool-call nodes this decision consolidates.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub consolidated_from: Vec<String>,
}

/// JSON representation of a causal edge between two provenance nodes.
#[derive(Debug, Clone, Serialize)]
pub struct JsonLedgerEdge {
    /// Source node ID (the cause).
    pub from: String,
    /// Target node ID (the effect).
    pub to: String,
    /// Edge kind label (snake_case: "led_to", "explored_via", ...).
    pub kind: String,
}

impl JsonChangeLedger {
    /// Build a JSON ledger from a provenance graph and its content hash.
    pub fn from_graph(graph_hash: &Hash, graph: &atomic_core::change::ProvenanceGraph) -> Self {
        Self {
            graph_hash: graph_hash.to_base32(),
            session_id: graph.session_id.clone(),
            agent_name: graph.agent_name.clone(),
            agent_display_name: graph.agent_display_name.clone(),
            agent_vendor: graph.agent_vendor.clone(),
            timestamp: graph.timestamp,
            profile: graph.profile.clone(),
            plan_id: graph.plan_id.clone(),
            node_count: graph.node_count(),
            edge_count: graph.edge_count(),
            change_count: graph.change_count(),
            changes_explained: graph
                .changes_explained
                .iter()
                .map(|h| h.to_base32())
                .collect(),
            previous: graph.previous.as_ref().map(|h| h.to_base32()),
            nodes: graph.nodes.iter().map(JsonLedgerNode::from).collect(),
            edges: graph.edges.iter().map(JsonLedgerEdge::from).collect(),
        }
    }
}

impl From<&atomic_core::change::provenance_graph::ProvenanceNode> for JsonLedgerNode {
    fn from(node: &atomic_core::change::provenance_graph::ProvenanceNode) -> Self {
        let detail = node.detail.as_ref().map(|d| {
            serde_json::from_str::<serde_json::Value>(d)
                .unwrap_or_else(|_| serde_json::Value::String(d.clone()))
        });
        Self {
            id: node.id.clone(),
            kind: node.kind.to_string(),
            timestamp: node.timestamp,
            summary: node.summary.clone(),
            detail,
            change_hash: node.change_hash.as_ref().map(|h| h.to_base32()),
            tool_name: node.tool_name.clone(),
            tool_call_id: node.tool_call_id.clone(),
            duration_ms: node.duration_ms,
            classified: node.classified,
            confidence: node.confidence,
            consolidated_from: node.consolidated_from.clone(),
        }
    }
}

impl From<&atomic_core::change::provenance_graph::ProvenanceEdge> for JsonLedgerEdge {
    fn from(edge: &atomic_core::change::provenance_graph::ProvenanceEdge) -> Self {
        Self {
            from: edge.from.clone(),
            to: edge.to.clone(),
            kind: edge.kind.to_string(),
        }
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
    /// Hash of the prompt (base32), when only the hash is stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_hash: Option<String>,
    /// Additional metadata key-value pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Agent mode: "build", "code", "ask", etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<String>,
    /// Why the model stopped on the final step ("stop", "tool-calls",
    /// "length").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Number of LLM roundtrips (steps) in the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_count: Option<u32>,
    /// Human-readable session slug (e.g., "mighty-rocket").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_slug: Option<String>,
    /// Model-provider signature over the reasoning blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
    /// Concatenated reasoning/thinking text from the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_text: Option<String>,
    /// Agent's structured task plan at turn completion (JSON string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_plan: Option<String>,
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

        let prompt_hash = match &prov.prompt {
            atomic_core::change::PromptContent::Hashed(h) => {
                use atomic_core::types::Base32;
                Some(h.to_base32())
            }
            _ => None,
        };

        let metadata = if prov.metadata.is_empty() {
            None
        } else {
            Some(serde_json::Value::Array(
                prov.metadata
                    .iter()
                    .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
                    .collect(),
            ))
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
            prompt_hash,
            metadata,
            agent_mode: prov.agent_mode.clone(),
            finish_reason: prov.finish_reason.clone(),
            step_count: prov.step_count,
            session_slug: prov.session_slug.clone(),
            reasoning_signature: prov.reasoning_signature.clone(),
            reasoning_text: prov.reasoning_text.clone(),
            task_plan: prov.task_plan.clone(),
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
