//! Attestation — graph-level audit node for AI cost, tokens, and compliance.
//!
//! An attestation is a content-addressed node in the graph that captures
//! metadata about a set of changes: cost, token usage per model, duration,
//! and agent identity. Unlike changes, attestations produce zero hunks —
//! they don't modify the content graph. Unlike tags, they're not tied to
//! a stack's state. They live in the graph as standalone nodes with
//! dependencies pointing to the changes they cover.
//!
//! # Graph Position
//!
//! ```text
//! Content Graph:    A ──▶ B ──▶ C ──▶ D
//!                                         (attestations are NOT here)
//!
//! Dependency Graph: A ◀── Attest₁
//!                   B ◀── Attest₁
//!                   C ◀── Attest₁
//!                   D ◀── Attest₂ ──▶ (previous: Attest₁)
//! ```
//!
//! Attestations are registered in EXTERNAL/INTERNAL (content-addressed),
//! NODE_TYPES (type = 2), and DEPS (attestation → covered changes).
//! They are NOT added to any stack's STACK_CHANGES table.
//!
//! # File Format
//!
//! Stored as `{hash}.attest` in `.atomic/changes/{prefix}/`:
//!
//! ```text
//! [MAGIC: 4 bytes "ATST"]
//! [postcard payload → Attestation]
//! ```
//!
//! # Cross-Stack Queries
//!
//! Given a stack, find attestations by:
//! 1. Iterate changes in the stack (STACK_CHANGES)
//! 2. For each change, check REV_DEPS for attestation nodes
//! 3. Filter by node_type = ATTESTATION
//! 4. Deduplicate (multiple changes may point to same attestation)
//! 5. Resolve which stacks contain each covered change
//!
//! # Session Resume Chaining
//!
//! When a session is resumed, a new attestation is created that covers
//! only the new changes. It links to the previous attestation via
//! `previous_attestation`, forming a chain:
//!
//! ```text
//! Attest₁ { deps: [A, B, C], previous: None }
//! Attest₂ { deps: [D, E], previous: Some(Attest₁) }
//! ```
//!
//! To compute session totals, walk the chain and sum.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::attestation::{Attestation, AttestAgent, ModelUsage, CodeChangeStats};
//! use atomic_core::types::Hash;
//!
//! let attest = Attestation::builder("session-123", AttestAgent::new("claude-code", "Claude Code", "anthropic"))
//!     .cost_usd(0.57)
//!     .duration_api_ms(172_000)
//!     .duration_wall_ms(2_354_000)
//!     .code_changes(CodeChangeStats { lines_added: 263, lines_removed: 8 })
//!     .add_model(ModelUsage {
//!         model: "claude-sonnet-4-5".into(),
//!         input_tokens: 176,
//!         output_tokens: 8400,
//!         cache_read_tokens: 526_900,
//!         cache_write_tokens: 13_100,
//!         cost_usd: 0.56,
//!     })
//!     .add_model(ModelUsage {
//!         model: "claude-haiku-4-5".into(),
//!         input_tokens: 9400,
//!         output_tokens: 403,
//!         cache_read_tokens: 0,
//!         cache_write_tokens: 0,
//!         cost_usd: 0.0115,
//!     })
//!     .add_change(Hash::of(b"change-a"))
//!     .add_change(Hash::of(b"change-b"))
//!     .build();
//!
//! // Serialize
//! let bytes = attest.serialize().unwrap();
//! assert!(Attestation::is_attestation(&bytes));
//!
//! // Deserialize and verify hash
//! let (loaded, hash) = Attestation::deserialize(&bytes).unwrap();
//! assert_eq!(loaded.session_id, "session-123");
//! assert_eq!(loaded.models.len(), 2);
//! assert_eq!(loaded.changes_covered.len(), 2);
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};

use crate::types::Hash;

// Constants

/// Magic bytes identifying an attestation file: "ATST"
const MAGIC: &[u8; 4] = b"ATST";

/// Current attestation schema version.
const SCHEMA_VERSION: u8 = 1;

/// File extension for attestation files.
pub const ATTESTATION_EXTENSION: &str = "attest";

// Attestation

/// A graph-level audit node capturing metadata about a set of changes.
///
/// Attestations are content-addressed (identified by Blake3 hash), stored
/// alongside change files, and registered in the graph with node_type = 2.
/// They have dependencies on the changes they cover but produce zero hunks.
///
/// # Fields
///
/// - `agent`: Who created these changes (name, display name, vendor)
/// - `session_id`: Links attestations within the same session
/// - `cost_usd`: Total cost across all models
/// - `duration_api_ms`: Time the API spent processing
/// - `duration_wall_ms`: Wall clock time of the session segment
/// - `code_changes`: Lines added/removed statistics
/// - `models`: Per-model token and cost breakdown
/// - `changes_covered`: Hashes of changes this attestation covers
/// - `previous_attestation`: Chain link for session resume
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attestation {
    /// Schema version for forward compatibility.
    pub version: u8,

    /// Timestamp when the attestation was created (Unix epoch seconds).
    pub timestamp: i64,

    /// The agent that produced the covered changes.
    pub agent: AttestAgent,

    /// Session identifier linking turns together.
    pub session_id: String,

    /// Total cost in USD across all models.
    pub cost_usd: f64,

    /// Duration the API spent processing (milliseconds).
    pub duration_api_ms: u64,

    /// Wall clock duration of the session segment (milliseconds).
    pub duration_wall_ms: u64,

    /// Code change statistics.
    pub code_changes: CodeChangeStats,

    /// Per-model token usage and cost breakdown.
    pub models: Vec<ModelUsage>,

    /// Hashes of the changes this attestation covers.
    ///
    /// Also registered in the DEPS table so the graph knows the
    /// relationship. Denormalized here for fast access.
    pub changes_covered: Vec<Hash>,

    /// Hash of the previous attestation in this session (if any).
    ///
    /// On session resume, a new attestation chains to the previous one.
    /// Walk the chain to compute cumulative session totals.
    #[serde(default)]
    pub previous_attestation: Option<Hash>,

    /// Optional free-form notes.
    #[serde(default)]
    pub notes: Option<String>,
}

impl Attestation {
    /// Create a builder for constructing an attestation.
    pub fn builder(session_id: impl Into<String>, agent: AttestAgent) -> AttestationBuilder {
        AttestationBuilder::new(session_id, agent)
    }

    /// Serialize the attestation to bytes.
    ///
    /// Format: `[MAGIC: 4 bytes][postcard payload]`
    ///
    /// The hash is computed over the entire serialized output.
    pub fn serialize(&self) -> Result<Vec<u8>, AttestationError> {
        let payload = postcard::to_allocvec(self).map_err(|e| AttestationError::Codec {
            reason: format!("postcard serialize failed: {}", e),
        })?;

        let mut buf = Vec::with_capacity(MAGIC.len() + payload.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&payload);
        Ok(buf)
    }

    /// Deserialize an attestation from bytes, returning the attestation and its hash.
    ///
    /// The hash is computed over the input bytes (the entire serialized form).
    pub fn deserialize(data: &[u8]) -> Result<(Self, Hash), AttestationError> {
        if data.len() < MAGIC.len() + 1 {
            return Err(AttestationError::Codec {
                reason: format!(
                    "data too short: {} bytes (minimum {})",
                    data.len(),
                    MAGIC.len() + 1
                ),
            });
        }

        if &data[..4] != MAGIC {
            return Err(AttestationError::Codec {
                reason: format!("invalid magic: expected {:?}, got {:?}", MAGIC, &data[..4]),
            });
        }

        let attestation: Self =
            postcard::from_bytes(&data[4..]).map_err(|e| AttestationError::Codec {
                reason: format!("postcard deserialize failed: {}", e),
            })?;

        if attestation.version > SCHEMA_VERSION {
            return Err(AttestationError::UnsupportedVersion {
                version: attestation.version,
                max_supported: SCHEMA_VERSION,
            });
        }

        let hash = Hash::of(data);
        Ok((attestation, hash))
    }

    /// Read an attestation from a reader (e.g., a file).
    pub fn read_from<R: Read>(reader: &mut R) -> Result<(Self, Hash), AttestationError> {
        let mut data = Vec::new();
        reader
            .read_to_end(&mut data)
            .map_err(|e| AttestationError::Io(e))?;
        Self::deserialize(&data)
    }

    /// Write an attestation to a writer (e.g., a file).
    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<Hash, AttestationError> {
        let data = self.serialize()?;
        let hash = Hash::of(&data);
        writer
            .write_all(&data)
            .map_err(|e| AttestationError::Io(e))?;
        Ok(hash)
    }

    /// Check whether a byte slice looks like an attestation file.
    ///
    /// Fast check — only inspects the 4-byte magic prefix.
    pub fn is_attestation(data: &[u8]) -> bool {
        data.len() >= MAGIC.len() && &data[..4] == MAGIC
    }

    /// Total tokens across all models (input + output + cache).
    pub fn total_tokens(&self) -> u64 {
        self.models.iter().map(|m| m.total_tokens()).sum()
    }

    /// Used tokens across all models (input + output only).
    ///
    /// This is the number that should be displayed as the headline "Tokens"
    /// metric — it matches what the user sees in OpenCode and represents
    /// actual computation, not cached context.
    pub fn used_tokens(&self) -> u64 {
        self.models.iter().map(|m| m.used_tokens()).sum()
    }

    /// Cache tokens across all models (cache read + cache write).
    pub fn cache_tokens(&self) -> u64 {
        self.models.iter().map(|m| m.cache_tokens()).sum()
    }

    /// Number of changes covered.
    pub fn change_count(&self) -> usize {
        self.changes_covered.len()
    }

    /// Check if a specific change is covered by this attestation.
    pub fn covers_change(&self, hash: &Hash) -> bool {
        self.changes_covered.contains(hash)
    }

    /// Check if this attestation is part of a chain (has a predecessor).
    pub fn is_chained(&self) -> bool {
        self.previous_attestation.is_some()
    }

    /// Human-readable display of the API duration.
    pub fn api_duration_display(&self) -> String {
        format_duration_ms(self.duration_api_ms)
    }

    /// Human-readable display of the wall clock duration.
    pub fn wall_duration_display(&self) -> String {
        format_duration_ms(self.duration_wall_ms)
    }

    /// Human-readable display of cost.
    pub fn cost_display(&self) -> String {
        if self.cost_usd < 0.01 {
            format!("${:.4}", self.cost_usd)
        } else {
            format!("${:.2}", self.cost_usd)
        }
    }
}

impl fmt::Display for Attestation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Attestation — {} · {} · {} tokens ({} cached) · {} API",
            self.agent.display_name,
            self.cost_display(),
            format_tokens(self.used_tokens()),
            format_tokens(self.cache_tokens()),
            self.api_duration_display(),
        )?;
        for model in &self.models {
            writeln!(f, "  {}", model)?;
        }
        writeln!(
            f,
            "  {} covered · +{} -{} lines",
            self.change_count(),
            self.code_changes.lines_added,
            self.code_changes.lines_removed,
        )?;
        Ok(())
    }
}

// AttestAgent

/// Identity of the agent that produced the covered changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestAgent {
    /// Agent registry key (e.g., "claude-code", "gemini-cli", "codex").
    pub name: String,
    /// Human-readable name (e.g., "Claude Code", "Gemini CLI").
    pub display_name: String,
    /// AI vendor (e.g., "anthropic", "google", "openai").
    pub vendor: String,
}

impl AttestAgent {
    /// Create a new agent identity.
    pub fn new(
        name: impl Into<String>,
        display_name: impl Into<String>,
        vendor: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            display_name: display_name.into(),
            vendor: vendor.into(),
        }
    }
}

impl fmt::Display for AttestAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.display_name, self.vendor)
    }
}

// CodeChangeStats

/// Code change statistics covered by the attestation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeChangeStats {
    /// Total lines added across all covered changes.
    pub lines_added: u64,
    /// Total lines removed across all covered changes.
    pub lines_removed: u64,
}

impl CodeChangeStats {
    /// Create stats with specific values.
    pub fn new(added: u64, removed: u64) -> Self {
        Self {
            lines_added: added,
            lines_removed: removed,
        }
    }

    /// Total line changes (additions + removals).
    pub fn total(&self) -> u64 {
        self.lines_added + self.lines_removed
    }

    /// Check if there were any changes.
    pub fn is_empty(&self) -> bool {
        self.lines_added == 0 && self.lines_removed == 0
    }
}

impl fmt::Display for CodeChangeStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "+{} -{}", self.lines_added, self.lines_removed)
    }
}

// ModelUsage

/// Token usage and cost for a single model within the attestation.
///
/// Mirrors the breakdown from `claude --resume`:
/// ```text
/// claude-sonnet-4-5: 176 input, 8.4k output, 526.9k cache read, 13.1k cache write ($0.56)
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Model identifier (e.g., "claude-sonnet-4-5", "claude-haiku-4-5").
    pub model: String,
    /// Input/prompt tokens.
    pub input_tokens: u64,
    /// Output/completion tokens.
    pub output_tokens: u64,
    /// Tokens read from cache.
    pub cache_read_tokens: u64,
    /// Tokens written to cache.
    pub cache_write_tokens: u64,
    /// Cost in USD for this model's usage.
    pub cost_usd: f64,
}

impl ModelUsage {
    /// Create a new model usage entry.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 0.0,
        }
    }

    /// Total tokens for this model (input + output + cache read + cache write).
    ///
    /// This is the raw total across all token categories. For display purposes,
    /// prefer `used_tokens()` (what was actually computed) vs `cache_tokens()`
    /// (what was served from cache).
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    /// Tokens that represent actual computation: input + output.
    ///
    /// This matches what OpenCode displays as "Context: N tokens" — the tokens
    /// the model actually processed and generated, excluding cached context.
    pub fn used_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Tokens served from or written to cache: cache_read + cache_write.
    ///
    /// Cache tokens represent reused context window — they reduce cost but
    /// don't represent new computation. Displayed separately from used tokens
    /// so users understand what they're paying for vs what was free/cheap.
    pub fn cache_tokens(&self) -> u64 {
        self.cache_read_tokens + self.cache_write_tokens
    }

    /// Set input tokens.
    pub fn with_input(mut self, tokens: u64) -> Self {
        self.input_tokens = tokens;
        self
    }

    /// Set output tokens.
    pub fn with_output(mut self, tokens: u64) -> Self {
        self.output_tokens = tokens;
        self
    }

    /// Set cache read tokens.
    pub fn with_cache_read(mut self, tokens: u64) -> Self {
        self.cache_read_tokens = tokens;
        self
    }

    /// Set cache write tokens.
    pub fn with_cache_write(mut self, tokens: u64) -> Self {
        self.cache_write_tokens = tokens;
        self
    }

    /// Set cost in USD.
    pub fn with_cost(mut self, usd: f64) -> Self {
        self.cost_usd = usd;
        self
    }
}

impl fmt::Display for ModelUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.model)?;

        let mut parts = Vec::new();
        if self.input_tokens > 0 {
            parts.push(format!("{} input", format_tokens(self.input_tokens)));
        }
        if self.output_tokens > 0 {
            parts.push(format!("{} output", format_tokens(self.output_tokens)));
        }
        if self.cache_read_tokens > 0 {
            parts.push(format!(
                "{} cache read",
                format_tokens(self.cache_read_tokens)
            ));
        }
        if self.cache_write_tokens > 0 {
            parts.push(format!(
                "{} cache write",
                format_tokens(self.cache_write_tokens)
            ));
        }

        write!(f, "{}", parts.join(", "))?;

        if self.cost_usd > 0.0 {
            if self.cost_usd < 0.01 {
                write!(f, " (${:.4})", self.cost_usd)?;
            } else {
                write!(f, " (${:.2})", self.cost_usd)?;
            }
        }

        Ok(())
    }
}

// AttestationBuilder

/// Builder for constructing `Attestation` instances.
pub struct AttestationBuilder {
    session_id: String,
    agent: AttestAgent,
    cost_usd: f64,
    duration_api_ms: u64,
    duration_wall_ms: u64,
    code_changes: CodeChangeStats,
    models: Vec<ModelUsage>,
    changes_covered: Vec<Hash>,
    previous_attestation: Option<Hash>,
    notes: Option<String>,
    timestamp: Option<i64>,
}

impl AttestationBuilder {
    /// Create a new builder with required fields.
    pub fn new(session_id: impl Into<String>, agent: AttestAgent) -> Self {
        Self {
            session_id: session_id.into(),
            agent,
            cost_usd: 0.0,
            duration_api_ms: 0,
            duration_wall_ms: 0,
            code_changes: CodeChangeStats::default(),
            models: Vec::new(),
            changes_covered: Vec::new(),
            previous_attestation: None,
            notes: None,
            timestamp: None,
        }
    }

    /// Set the total cost in USD.
    pub fn cost_usd(mut self, cost: f64) -> Self {
        self.cost_usd = cost;
        self
    }

    /// Set the API processing duration in milliseconds.
    pub fn duration_api_ms(mut self, ms: u64) -> Self {
        self.duration_api_ms = ms;
        self
    }

    /// Set the wall clock duration in milliseconds.
    pub fn duration_wall_ms(mut self, ms: u64) -> Self {
        self.duration_wall_ms = ms;
        self
    }

    /// Set the code change statistics.
    pub fn code_changes(mut self, stats: CodeChangeStats) -> Self {
        self.code_changes = stats;
        self
    }

    /// Add a model usage entry.
    pub fn add_model(mut self, usage: ModelUsage) -> Self {
        self.models.push(usage);
        self
    }

    /// Set all model usage entries at once.
    pub fn models(mut self, models: Vec<ModelUsage>) -> Self {
        self.models = models;
        self
    }

    /// Add a covered change hash.
    pub fn add_change(mut self, hash: Hash) -> Self {
        self.changes_covered.push(hash);
        self
    }

    /// Set all covered change hashes at once.
    pub fn changes_covered(mut self, hashes: Vec<Hash>) -> Self {
        self.changes_covered = hashes;
        self
    }

    /// Set the previous attestation hash (for session resume chaining).
    pub fn previous_attestation(mut self, hash: Hash) -> Self {
        self.previous_attestation = Some(hash);
        self
    }

    /// Set optional notes.
    pub fn notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = Some(notes.into());
        self
    }

    /// Set the timestamp (Unix epoch seconds). Defaults to now.
    pub fn timestamp(mut self, ts: i64) -> Self {
        self.timestamp = Some(ts);
        self
    }

    /// Build the attestation.
    pub fn build(self) -> Attestation {
        let timestamp = self.timestamp.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });

        Attestation {
            version: SCHEMA_VERSION,
            timestamp,
            agent: self.agent,
            session_id: self.session_id,
            cost_usd: self.cost_usd,
            duration_api_ms: self.duration_api_ms,
            duration_wall_ms: self.duration_wall_ms,
            code_changes: self.code_changes,
            models: self.models,
            changes_covered: self.changes_covered,
            previous_attestation: self.previous_attestation,
            notes: self.notes,
        }
    }
}

// Error Type

/// Errors that can occur during attestation operations.
#[derive(Debug, thiserror::Error)]
pub enum AttestationError {
    /// Serialization or deserialization failed.
    #[error("Attestation codec error: {reason}")]
    Codec {
        /// Description of what went wrong.
        reason: String,
    },

    /// The attestation version is not supported.
    #[error("Unsupported attestation version: {version} (max supported: {max_supported})")]
    UnsupportedVersion {
        /// The version found in the data.
        version: u8,
        /// The maximum version this code supports.
        max_supported: u8,
    },

    /// I/O error reading or writing the attestation.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

// Formatting Helpers

/// Format a token count with K/M suffixes.
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{}", tokens)
    }
}

/// Format a duration in milliseconds to a human-readable string.
fn format_duration_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    if total_secs >= 3600 {
        let hrs = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        let secs = total_secs % 60;
        if secs > 0 {
            format!("{}h {}m {}s", hrs, mins, secs)
        } else {
            format!("{}h {}m", hrs, mins)
        }
    } else if total_secs >= 60 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        if secs > 0 {
            format!("{}m {}s", mins, secs)
        } else {
            format!("{}m", mins)
        }
    } else if total_secs > 0 {
        format!("{}s", total_secs)
    } else {
        format!("{}ms", ms)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Hash;

    fn make_agent() -> AttestAgent {
        AttestAgent::new("claude-code", "Claude Code", "anthropic")
    }

    fn make_attestation() -> Attestation {
        Attestation::builder("session-abc", make_agent())
            .cost_usd(0.57)
            .duration_api_ms(172_000)
            .duration_wall_ms(2_354_000)
            .code_changes(CodeChangeStats::new(263, 8))
            .add_model(ModelUsage {
                model: "claude-sonnet-4-5".into(),
                input_tokens: 176,
                output_tokens: 8400,
                cache_read_tokens: 526_900,
                cache_write_tokens: 13_100,
                cost_usd: 0.56,
            })
            .add_model(ModelUsage {
                model: "claude-haiku-4-5".into(),
                input_tokens: 9400,
                output_tokens: 403,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.0115,
            })
            .add_change(Hash::of(b"change-a"))
            .add_change(Hash::of(b"change-b"))
            .add_change(Hash::of(b"change-c"))
            .timestamp(1739290034)
            .build()
    }

    // -------------------------------------------------------------------------
    // Agent
    // -------------------------------------------------------------------------

    #[test]
    fn test_agent_new() {
        let agent = AttestAgent::new("claude-code", "Claude Code", "anthropic");
        assert_eq!(agent.name, "claude-code");
        assert_eq!(agent.display_name, "Claude Code");
        assert_eq!(agent.vendor, "anthropic");
    }

    #[test]
    fn test_agent_display() {
        let agent = make_agent();
        assert_eq!(format!("{}", agent), "Claude Code (anthropic)");
    }

    // -------------------------------------------------------------------------
    // CodeChangeStats
    // -------------------------------------------------------------------------

    #[test]
    fn test_code_stats_new() {
        let stats = CodeChangeStats::new(100, 20);
        assert_eq!(stats.lines_added, 100);
        assert_eq!(stats.lines_removed, 20);
        assert_eq!(stats.total(), 120);
        assert!(!stats.is_empty());
    }

    #[test]
    fn test_code_stats_empty() {
        let stats = CodeChangeStats::default();
        assert!(stats.is_empty());
        assert_eq!(stats.total(), 0);
    }

    #[test]
    fn test_code_stats_display() {
        let stats = CodeChangeStats::new(263, 8);
        assert_eq!(format!("{}", stats), "+263 -8");
    }

    // -------------------------------------------------------------------------
    // ModelUsage
    // -------------------------------------------------------------------------

    #[test]
    fn test_model_usage_new() {
        let m = ModelUsage::new("claude-sonnet-4-5");
        assert_eq!(m.model, "claude-sonnet-4-5");
        assert_eq!(m.total_tokens(), 0);
    }

    #[test]
    fn test_model_usage_builder() {
        let m = ModelUsage::new("claude-sonnet-4-5")
            .with_input(176)
            .with_output(8400)
            .with_cache_read(526_900)
            .with_cache_write(13_100)
            .with_cost(0.56);

        assert_eq!(m.input_tokens, 176);
        assert_eq!(m.output_tokens, 8400);
        assert_eq!(m.cache_read_tokens, 526_900);
        assert_eq!(m.cache_write_tokens, 13_100);
        assert_eq!(m.cost_usd, 0.56);
        assert_eq!(m.total_tokens(), 176 + 8400 + 526_900 + 13_100);
    }

    #[test]
    fn test_model_usage_display() {
        let m = ModelUsage::new("claude-sonnet-4-5")
            .with_input(176)
            .with_output(8400)
            .with_cache_read(526_900)
            .with_cost(0.56);

        let display = format!("{}", m);
        assert!(display.contains("claude-sonnet-4-5"));
        assert!(display.contains("input"));
        assert!(display.contains("output"));
        assert!(display.contains("cache read"));
        assert!(display.contains("$0.56"));
    }

    #[test]
    fn test_model_usage_display_small_cost() {
        let m = ModelUsage::new("haiku").with_input(100).with_cost(0.0015);

        let display = format!("{}", m);
        assert!(display.contains("$0.0015"));
    }

    // -------------------------------------------------------------------------
    // Attestation Builder
    // -------------------------------------------------------------------------

    #[test]
    fn test_builder_minimal() {
        let a = Attestation::builder("sess-1", make_agent()).build();
        assert_eq!(a.session_id, "sess-1");
        assert_eq!(a.agent.name, "claude-code");
        assert_eq!(a.version, SCHEMA_VERSION);
        assert_eq!(a.cost_usd, 0.0);
        assert!(a.models.is_empty());
        assert!(a.changes_covered.is_empty());
        assert!(a.previous_attestation.is_none());
        assert!(a.notes.is_none());
    }

    #[test]
    fn test_builder_full() {
        let a = make_attestation();
        assert_eq!(a.session_id, "session-abc");
        assert_eq!(a.cost_usd, 0.57);
        assert_eq!(a.duration_api_ms, 172_000);
        assert_eq!(a.duration_wall_ms, 2_354_000);
        assert_eq!(a.code_changes.lines_added, 263);
        assert_eq!(a.code_changes.lines_removed, 8);
        assert_eq!(a.models.len(), 2);
        assert_eq!(a.changes_covered.len(), 3);
        assert_eq!(a.timestamp, 1739290034);
    }

    #[test]
    fn test_builder_with_previous() {
        let prev = Hash::of(b"prev-attest");
        let a = Attestation::builder("sess-2", make_agent())
            .previous_attestation(prev)
            .build();

        assert!(a.is_chained());
        assert_eq!(a.previous_attestation, Some(prev));
    }

    #[test]
    fn test_builder_with_notes() {
        let a = Attestation::builder("sess", make_agent())
            .notes("Session was interrupted and resumed")
            .build();

        assert_eq!(
            a.notes.as_deref(),
            Some("Session was interrupted and resumed")
        );
    }

    #[test]
    fn test_builder_changes_covered_vec() {
        let hashes = vec![Hash::of(b"a"), Hash::of(b"b")];
        let a = Attestation::builder("sess", make_agent())
            .changes_covered(hashes.clone())
            .build();

        assert_eq!(a.changes_covered, hashes);
    }

    #[test]
    fn test_builder_models_vec() {
        let models = vec![
            ModelUsage::new("model-a").with_cost(0.10),
            ModelUsage::new("model-b").with_cost(0.20),
        ];
        let a = Attestation::builder("sess", make_agent())
            .models(models)
            .cost_usd(0.30)
            .build();

        assert_eq!(a.models.len(), 2);
    }

    // -------------------------------------------------------------------------
    // Serialization
    // -------------------------------------------------------------------------

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let original = make_attestation();
        let bytes = original.serialize().unwrap();
        let (loaded, _hash) = Attestation::deserialize(&bytes).unwrap();

        assert_eq!(original, loaded);
    }

    #[test]
    fn test_serialize_has_magic() {
        let a = make_attestation();
        let bytes = a.serialize().unwrap();

        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], b"ATST");
    }

    #[test]
    fn test_is_attestation_valid() {
        let bytes = make_attestation().serialize().unwrap();
        assert!(Attestation::is_attestation(&bytes));
    }

    #[test]
    fn test_is_attestation_too_short() {
        assert!(!Attestation::is_attestation(b"AT"));
        assert!(!Attestation::is_attestation(b""));
    }

    #[test]
    fn test_is_attestation_wrong_magic() {
        assert!(!Attestation::is_attestation(b"ABCD1234"));
        assert!(!Attestation::is_attestation(b"ATSE1234"));
    }

    #[test]
    fn test_deserialize_too_short() {
        let result = Attestation::deserialize(b"AT");
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_wrong_magic() {
        let result = Attestation::deserialize(b"NOPE____data");
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_corrupted_payload() {
        let mut bytes = b"ATST".to_vec();
        bytes.extend_from_slice(b"not valid postcard data at all");
        let result = Attestation::deserialize(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_hash_deterministic() {
        let a = make_attestation();
        let bytes1 = a.serialize().unwrap();
        let bytes2 = a.serialize().unwrap();

        let (_, hash1) = Attestation::deserialize(&bytes1).unwrap();
        let (_, hash2) = Attestation::deserialize(&bytes2).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_different_content_different_hash() {
        let a1 = Attestation::builder("sess-1", make_agent())
            .cost_usd(0.50)
            .timestamp(1000)
            .build();
        let a2 = Attestation::builder("sess-2", make_agent())
            .cost_usd(1.00)
            .timestamp(1000)
            .build();

        let bytes1 = a1.serialize().unwrap();
        let bytes2 = a2.serialize().unwrap();

        let (_, hash1) = Attestation::deserialize(&bytes1).unwrap();
        let (_, hash2) = Attestation::deserialize(&bytes2).unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_minimal_roundtrip() {
        let a = Attestation::builder("s", make_agent()).timestamp(0).build();
        let bytes = a.serialize().unwrap();
        let (loaded, _) = Attestation::deserialize(&bytes).unwrap();
        assert_eq!(loaded.session_id, "s");
        assert!(loaded.models.is_empty());
        assert!(loaded.changes_covered.is_empty());
    }

    #[test]
    fn test_write_read_roundtrip() {
        let original = make_attestation();
        let mut buf = Vec::new();
        let write_hash = original.write_to(&mut buf).unwrap();

        let mut cursor = std::io::Cursor::new(&buf);
        let (loaded, read_hash) = Attestation::read_from(&mut cursor).unwrap();

        assert_eq!(original, loaded);
        assert_eq!(write_hash, read_hash);
    }

    // -------------------------------------------------------------------------
    // Query Helpers
    // -------------------------------------------------------------------------

    #[test]
    fn test_total_tokens() {
        let a = make_attestation();
        // sonnet: 176 + 8400 + 526900 + 13100 = 548576
        // haiku:  9400 + 403 + 0 + 0 = 9803
        assert_eq!(a.total_tokens(), 548_576 + 9_803);
    }

    #[test]
    fn test_used_tokens() {
        let a = make_attestation();
        // used = input + output only (no cache)
        // sonnet: 176 + 8400 = 8576
        // haiku:  9400 + 403 = 9803
        assert_eq!(a.used_tokens(), 8_576 + 9_803);
    }

    #[test]
    fn test_cache_tokens() {
        let a = make_attestation();
        // cache = cache_read + cache_write
        // sonnet: 526900 + 13100 = 540000
        // haiku:  0 + 0 = 0
        assert_eq!(a.cache_tokens(), 540_000);
    }

    #[test]
    fn test_used_plus_cache_equals_total() {
        let a = make_attestation();
        assert_eq!(a.used_tokens() + a.cache_tokens(), a.total_tokens());
    }

    #[test]
    fn test_change_count() {
        let a = make_attestation();
        assert_eq!(a.change_count(), 3);
    }

    #[test]
    fn test_covers_change() {
        let hash_a = Hash::of(b"change-a");
        let hash_z = Hash::of(b"change-z");
        let a = make_attestation();

        assert!(a.covers_change(&hash_a));
        assert!(!a.covers_change(&hash_z));
    }

    #[test]
    fn test_is_chained_false() {
        let a = make_attestation();
        assert!(!a.is_chained());
    }

    #[test]
    fn test_is_chained_true() {
        let a = Attestation::builder("s", make_agent())
            .previous_attestation(Hash::of(b"prev"))
            .build();
        assert!(a.is_chained());
    }

    // -------------------------------------------------------------------------
    // Display
    // -------------------------------------------------------------------------

    #[test]
    fn test_cost_display_small() {
        let a = Attestation::builder("s", make_agent())
            .cost_usd(0.0057)
            .build();
        assert_eq!(a.cost_display(), "$0.0057");
    }

    #[test]
    fn test_cost_display_normal() {
        let a = Attestation::builder("s", make_agent())
            .cost_usd(1.23)
            .build();
        assert_eq!(a.cost_display(), "$1.23");
    }

    #[test]
    fn test_api_duration_display() {
        let a = Attestation::builder("s", make_agent())
            .duration_api_ms(172_000)
            .build();
        assert_eq!(a.api_duration_display(), "2m 52s");
    }

    #[test]
    fn test_wall_duration_display() {
        let a = Attestation::builder("s", make_agent())
            .duration_wall_ms(2_354_000)
            .build();
        assert_eq!(a.wall_duration_display(), "39m 14s");
    }

    #[test]
    fn test_attestation_display() {
        let a = make_attestation();
        let display = format!("{}", a);
        assert!(display.contains("Claude Code"));
        assert!(display.contains("$0.57"));
        assert!(display.contains("3 covered"));
        assert!(display.contains("+263 -8"));
    }

    // -------------------------------------------------------------------------
    // Format Helpers
    // -------------------------------------------------------------------------

    #[test]
    fn test_format_tokens_small() {
        assert_eq!(format_tokens(42), "42");
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn test_format_tokens_thousands() {
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(8_400), "8.4k");
        assert_eq!(format_tokens(526_900), "526.9k");
    }

    #[test]
    fn test_format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn test_format_duration_ms() {
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(1_000), "1s");
        assert_eq!(format_duration_ms(45_000), "45s");
        assert_eq!(format_duration_ms(60_000), "1m");
        assert_eq!(format_duration_ms(172_000), "2m 52s");
        assert_eq!(format_duration_ms(2_354_000), "39m 14s");
        assert_eq!(format_duration_ms(3_600_000), "1h 0m");
        assert_eq!(format_duration_ms(3_661_000), "1h 1m 1s");
    }
}
