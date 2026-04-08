//! AI Provenance tracking for changes.
//!
//! This module provides structures for tracking the provenance of AI-assisted
//! changes, enabling transparency and accountability in AI-human collaborative
//! development workflows.
//!
//! # Module Structure
//!
//! - [`types`]: Vendor, tool, suggestion type, token usage, cost, prompt content
//! - [`builder`]: `ProvenanceBuilder` for fluent construction
//!
//! # Overview
//!
//! When AI agents (like Claude, GPT-4, Copilot, etc.) contribute to code changes,
//! it's important to track:
//!
//! - **What** prompted the AI (the input)
//! - **Who** the AI vendor/model was
//! - **How** the AI was used (tool, interaction type)
//! - **Cost** of the generation (tokens, compute)
//!
//! # Privacy Considerations
//!
//! Prompts may contain sensitive information (proprietary code context,
//! internal documentation, etc.). The module supports two modes:
//!
//! 1. **Hashed prompts**: Store only a hash of the prompt for verification
//! 2. **Full prompts**: Store the complete prompt (opt-in, compressed)
//!
//! The default is hashed prompts for privacy.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::{Provenance, AIVendor, AITool, SuggestionType};
//!
//! let provenance = Provenance::builder()
//!     .vendor(AIVendor::Anthropic)
//!     .model("claude-sonnet-4-20250514")
//!     .tool(AITool::Editor("zed".to_string()))
//!     .suggestion_type(SuggestionType::Collaborative)
//!     .prompt_hash("ABCDEF...") // Hash of the prompt for privacy
//!     .input_tokens(1500)
//!     .output_tokens(500)
//!     .cost_usd(0.015)
//!     .build();
//! ```

pub mod builder;
pub mod types;

#[cfg(test)]
mod tests;

// Re-export all public items so the parent `change` module sees the same API.
pub use builder::ProvenanceBuilder;
pub use types::{AITool, AIVendor, Cost, PromptContent, SuggestionType, TokenUsage};

use crate::Hash;
use serde::{Deserialize, Serialize};
use std::fmt;

// ============================================================================
// PROVENANCE
// ============================================================================

/// Complete provenance information for an AI-assisted change.
///
/// This structure captures all relevant information about AI involvement
/// in creating a change, enabling:
///
/// - **Attribution**: Who/what AI contributed
/// - **Auditing**: How much AI was used, at what cost
/// - **Verification**: Prompt hashes for integrity checking
/// - **Compliance**: Meeting AI disclosure requirements
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provenance {
    /// AI vendor/provider
    pub vendor: AIVendor,

    /// Specific model identifier (e.g., "claude-sonnet-4-20250514")
    pub model: String,

    /// Model version or checkpoint (optional, for fine-tuned models)
    #[serde(default)]
    pub model_version: Option<String>,

    /// Tool used to interact with the AI
    pub tool: AITool,

    /// Type of AI contribution
    pub suggestion_type: SuggestionType,

    /// The prompt (hashed or full)
    #[serde(default)]
    pub prompt: PromptContent,

    /// System prompt hash (if separate from main prompt)
    #[serde(default)]
    pub system_prompt_hash: Option<Hash>,

    /// Token usage
    #[serde(default)]
    pub tokens: TokenUsage,

    /// Cost of this generation
    #[serde(default)]
    pub cost: Cost,

    /// Temperature setting used (if applicable)
    /// Stored as fixed-point (value * 1000) for deterministic equality
    #[serde(default)]
    pub temperature: Option<u32>,

    /// Request timestamp (Unix epoch seconds)
    #[serde(default)]
    pub timestamp: Option<i64>,

    /// Request ID from the AI provider (for auditing)
    #[serde(default)]
    pub request_id: Option<String>,

    /// Session/conversation ID (if part of multi-turn interaction)
    #[serde(default)]
    pub session_id: Option<String>,

    /// Additional metadata (provider-specific key-value pairs)
    #[serde(default)]
    pub metadata: Vec<(String, String)>,

    // =========================================================================
    // Agent context fields — added for rich provenance from coding agents
    // =========================================================================
    /// Agent mode: "build", "code", "ask", etc.
    ///
    /// Determines the accountability context:
    /// - "build" = agent has full autonomy to create/edit/run
    /// - "code" = agent can edit but doesn't run commands
    /// - "ask" = advisory only, no file modifications
    #[serde(default)]
    pub agent_mode: Option<String>,

    /// Why the model stopped generating on the final step of this turn.
    ///
    /// - "stop" = agent decided it was done
    /// - "tool-calls" = agent wanted to execute tools (multi-step)
    /// - "length" = context window exhausted
    #[serde(default)]
    pub finish_reason: Option<String>,

    /// Number of LLM roundtrips (steps) in this turn.
    ///
    /// Each step is one model invocation. A turn with 14 steps means the
    /// model was called 14 times (interleaved with tool executions).
    #[serde(default)]
    pub step_count: Option<u32>,

    /// Human-readable session slug (e.g., "mighty-rocket").
    ///
    /// Assigned by the coding agent (OpenCode), useful for display and
    /// correlation. More memorable than the session UUID.
    #[serde(default)]
    pub session_slug: Option<String>,

    /// Cryptographic signature from the model provider on reasoning blocks.
    ///
    /// Currently Anthropic-specific: proves the chain-of-thought was genuinely
    /// produced by the model, not fabricated or tampered with. Stored as the
    /// last complete reasoning block's signature from the turn.
    ///
    /// This is a key differentiator for provenance: it's cryptographic proof
    /// from the model vendor that the reasoning is authentic.
    #[serde(default)]
    pub reasoning_signature: Option<String>,

    /// Concatenated reasoning/thinking text from all reasoning blocks in the turn.
    ///
    /// Contains the agent's chain-of-thought: why it made the decisions it did,
    /// what alternatives it considered, and how it planned its approach. Each
    /// reasoning block is separated by "\n---\n".
    ///
    /// Truncated to 10KB to avoid bloating change files. For the full text,
    /// see the ProvenanceGraph's Decision nodes.
    #[serde(default)]
    pub reasoning_text: Option<String>,

    /// Agent's structured task plan at turn completion.
    ///
    /// A snapshot of the agent's todo list when the turn finished. Each entry
    /// has `content` (task description), `status` ("pending", "in_progress",
    /// "completed", "cancelled"), and `priority` ("high", "medium", "low").
    ///
    /// Stored as a JSON string for flexibility.
    #[serde(default)]
    pub task_plan: Option<String>,
}

impl Provenance {
    /// Create a new provenance with minimal required fields.
    pub fn new(vendor: AIVendor, model: impl Into<String>, tool: AITool) -> Self {
        Self {
            vendor,
            model: model.into(),
            model_version: None,
            tool,
            suggestion_type: SuggestionType::default(),
            prompt: PromptContent::None,
            system_prompt_hash: None,
            tokens: TokenUsage::default(),
            cost: Cost::zero(),
            temperature: None,
            timestamp: None,
            request_id: None,
            session_id: None,
            metadata: Vec::new(),
            agent_mode: None,
            finish_reason: None,
            step_count: None,
            session_slug: None,
            reasoning_signature: None,
            reasoning_text: None,
            task_plan: None,
        }
    }

    /// Create a builder for constructing provenance.
    pub fn builder() -> ProvenanceBuilder {
        ProvenanceBuilder::default()
    }

    /// Set the prompt from text (hashed for privacy).
    pub fn with_prompt_hashed(mut self, prompt: &str) -> Self {
        self.prompt = PromptContent::hash_from(prompt);
        self
    }

    /// Set the full prompt text.
    pub fn with_prompt_full(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = PromptContent::full(prompt);
        self
    }

    /// Set token usage.
    pub fn with_tokens(mut self, input: u64, output: u64) -> Self {
        self.tokens = TokenUsage::new(input, output);
        self
    }

    /// Set cost in USD.
    pub fn with_cost_usd(mut self, usd: f64) -> Self {
        self.cost = Cost::from_usd(usd);
        self
    }

    /// Get a short summary string.
    pub fn summary(&self) -> String {
        format!(
            "{} {} via {} ({})",
            self.vendor, self.model, self.tool, self.suggestion_type
        )
    }

    /// Check if this provenance has cost information.
    pub fn has_cost(&self) -> bool {
        !self.cost.is_zero()
    }

    /// Check if this provenance has token information.
    pub fn has_tokens(&self) -> bool {
        !self.tokens.is_empty()
    }
}

impl Default for Provenance {
    fn default() -> Self {
        Self {
            vendor: AIVendor::default(),
            model: String::new(),
            model_version: None,
            tool: AITool::default(),
            suggestion_type: SuggestionType::default(),
            prompt: PromptContent::None,
            system_prompt_hash: None,
            tokens: TokenUsage::default(),
            cost: Cost::zero(),
            temperature: None,
            timestamp: None,
            request_id: None,
            session_id: None,
            metadata: Vec::new(),
            agent_mode: None,
            finish_reason: None,
            step_count: None,
            session_slug: None,
            reasoning_signature: None,
            reasoning_text: None,
            task_plan: None,
        }
    }
}

impl PartialEq for Provenance {
    fn eq(&self, other: &Self) -> bool {
        self.vendor == other.vendor
            && self.model == other.model
            && self.model_version == other.model_version
            && self.tool == other.tool
            && self.suggestion_type == other.suggestion_type
            && self.prompt == other.prompt
            && self.system_prompt_hash == other.system_prompt_hash
            && self.tokens == other.tokens
            && self.cost == other.cost
            && self.temperature == other.temperature
            && self.timestamp == other.timestamp
            && self.request_id == other.request_id
            && self.session_id == other.session_id
            && self.metadata == other.metadata
            && self.agent_mode == other.agent_mode
            && self.finish_reason == other.finish_reason
            && self.step_count == other.step_count
            && self.session_slug == other.session_slug
            && self.reasoning_signature == other.reasoning_signature
            && self.reasoning_text == other.reasoning_text
            && self.task_plan == other.task_plan
    }
}

impl Eq for Provenance {}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())?;
        if let Some(ref mode) = self.agent_mode {
            write!(f, " [{}]", mode)?;
        }
        if self.has_tokens() {
            write!(f, " - {}", self.tokens)?;
        }
        if self.has_cost() {
            write!(f, " - {}", self.cost)?;
        }
        if let Some(steps) = self.step_count {
            write!(f, " ({} steps)", steps)?;
        }
        if self.reasoning_signature.is_some() {
            write!(f, " ✓signed")?;
        }
        Ok(())
    }
}
