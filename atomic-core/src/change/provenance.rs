//! AI Provenance tracking for changes
//!
//! This module provides structures for tracking the provenance of AI-assisted
//! changes, enabling transparency and accountability in AI-human collaborative
//! development workflows.
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
//! This information is stored in the `Provenance` struct and can be included
//! in the hashed portion of a change, making AI attribution part of the
//! cryptographic identity of the change.
//!
//! # Design Principles
//!
//! 1. **Privacy-Aware**: Prompts can be hashed instead of stored verbatim
//! 2. **Flexible**: Supports various AI providers and interaction patterns
//! 3. **Auditable**: All fields contribute to change hash for integrity
//! 4. **Optional**: Changes can have zero, one, or multiple provenance entries
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

use crate::types::Base32;
use crate::Hash;
use serde::{Deserialize, Serialize};
use std::fmt;

/// AI vendor/provider identifiers.
///
/// This enum identifies the AI service provider. Using an enum ensures
/// consistent naming and enables vendor-specific handling if needed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AIVendor {
    /// Anthropic (Claude models)
    Anthropic,
    /// OpenAI (GPT models)
    OpenAI,
    /// Google (Gemini models)
    Google,
    /// Meta (Llama models)
    Meta,
    /// Mistral AI
    Mistral,
    /// Cohere
    Cohere,
    /// Amazon Bedrock (may host various models)
    AmazonBedrock,
    /// Azure OpenAI Service
    AzureOpenAI,
    /// Local/self-hosted model
    Local,
    /// Other/custom provider
    Other(String),
}

impl AIVendor {
    /// Get the canonical name for this vendor.
    pub fn name(&self) -> &str {
        match self {
            AIVendor::Anthropic => "anthropic",
            AIVendor::OpenAI => "openai",
            AIVendor::Google => "google",
            AIVendor::Meta => "meta",
            AIVendor::Mistral => "mistral",
            AIVendor::Cohere => "cohere",
            AIVendor::AmazonBedrock => "amazon-bedrock",
            AIVendor::AzureOpenAI => "azure-openai",
            AIVendor::Local => "local",
            AIVendor::Other(name) => name,
        }
    }

    /// Parse a vendor from a string.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "anthropic" | "claude" => AIVendor::Anthropic,
            "openai" | "gpt" | "chatgpt" => AIVendor::OpenAI,
            "google" | "gemini" | "bard" => AIVendor::Google,
            "meta" | "llama" | "facebook" => AIVendor::Meta,
            "mistral" => AIVendor::Mistral,
            "cohere" => AIVendor::Cohere,
            "bedrock" | "amazon-bedrock" | "aws" => AIVendor::AmazonBedrock,
            "azure" | "azure-openai" => AIVendor::AzureOpenAI,
            "local" | "self-hosted" | "ollama" => AIVendor::Local,
            other => AIVendor::Other(other.to_string()),
        }
    }
}

impl fmt::Display for AIVendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl Default for AIVendor {
    fn default() -> Self {
        AIVendor::Other("unknown".to_string())
    }
}

/// The tool or interface used to interact with the AI.
///
/// This captures HOW the AI was accessed, which can affect the nature
/// of the interaction and the context available to the AI.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AITool {
    /// Direct API access
    Api,
    /// Chat interface (web or app)
    Chat,
    /// Code editor integration
    Editor(String),
    /// IDE plugin
    IdePlugin(String),
    /// Command-line tool
    Cli(String),
    /// Continuous integration system
    CI(String),
    /// Code review tool
    CodeReview(String),
    /// Custom/other tool
    Other(String),
}

impl AITool {
    /// Get a description of this tool.
    pub fn description(&self) -> String {
        match self {
            AITool::Api => "API".to_string(),
            AITool::Chat => "Chat".to_string(),
            AITool::Editor(name) => format!("Editor: {}", name),
            AITool::IdePlugin(name) => format!("IDE Plugin: {}", name),
            AITool::Cli(name) => format!("CLI: {}", name),
            AITool::CI(name) => format!("CI: {}", name),
            AITool::CodeReview(name) => format!("Code Review: {}", name),
            AITool::Other(name) => format!("Other: {}", name),
        }
    }

    /// Create an editor tool variant.
    pub fn editor(name: impl Into<String>) -> Self {
        AITool::Editor(name.into())
    }

    /// Create a CLI tool variant.
    pub fn cli(name: impl Into<String>) -> Self {
        AITool::Cli(name.into())
    }

    /// Create an IDE plugin variant.
    pub fn ide_plugin(name: impl Into<String>) -> Self {
        AITool::IdePlugin(name.into())
    }
}

impl Default for AITool {
    fn default() -> Self {
        AITool::Other("unknown".to_string())
    }
}

impl fmt::Display for AITool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// The type of AI suggestion/contribution.
///
/// This categorizes how the AI contributed to the change, which is
/// important for understanding the human-AI collaboration dynamic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionType {
    /// AI generated the entire change with minimal human input
    Complete,
    /// AI suggested, human significantly modified
    Partial,
    /// Human started, AI completed/extended
    #[default]
    Collaborative,
    /// AI provided multiple options, human selected
    Selection,
    /// AI reviewed/improved human-written code
    Review,
    /// AI explained/documented existing code
    Documentation,
    /// AI helped debug/fix issues
    Debugging,
    /// AI refactored existing code
    Refactoring,
    /// AI generated tests
    Testing,
}

impl SuggestionType {
    /// Get a human-readable description.
    pub fn description(&self) -> &'static str {
        match self {
            SuggestionType::Complete => "AI-generated (complete)",
            SuggestionType::Partial => "AI-suggested, human-modified",
            SuggestionType::Collaborative => "Human-AI collaborative",
            SuggestionType::Selection => "Human-selected from AI options",
            SuggestionType::Review => "AI-reviewed",
            SuggestionType::Documentation => "AI-documented",
            SuggestionType::Debugging => "AI-assisted debugging",
            SuggestionType::Refactoring => "AI-assisted refactoring",
            SuggestionType::Testing => "AI-generated tests",
        }
    }

    /// Estimate the human contribution level (0.0 = all AI, 1.0 = all human).
    pub fn human_contribution_estimate(&self) -> f32 {
        match self {
            SuggestionType::Complete => 0.1,
            SuggestionType::Partial => 0.5,
            SuggestionType::Collaborative => 0.5,
            SuggestionType::Selection => 0.3,
            SuggestionType::Review => 0.7,
            SuggestionType::Documentation => 0.2,
            SuggestionType::Debugging => 0.4,
            SuggestionType::Refactoring => 0.3,
            SuggestionType::Testing => 0.2,
        }
    }
}

impl fmt::Display for SuggestionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Token usage information for an AI generation.
///
/// Tracks the computational resources used, which is important for:
/// - Cost attribution
/// - Understanding context window usage
/// - Auditing AI usage patterns
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Number of input/prompt tokens
    pub input_tokens: u64,
    /// Number of output/completion tokens
    pub output_tokens: u64,
    /// Total tokens (input + output)
    pub total_tokens: u64,
    /// Cache read tokens (if applicable)
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Cache write tokens (if applicable)
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Reasoning/thinking tokens (extended thinking, o1/o3, chain-of-thought)
    ///
    /// Separate billing category for models that expose reasoning tokens.
    /// For Anthropic extended thinking, the reasoning text arrives as
    /// `ReasoningPart` events but tokens may be billed under `output_tokens`
    /// rather than this field (model-dependent). For OpenAI o1/o3, reasoning
    /// tokens are billed separately and reported here.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl TokenUsage {
    /// Create new token usage with input and output counts.
    pub fn new(input: u64, output: u64) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        }
    }

    /// Create token usage with cache information.
    pub fn with_cache(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            reasoning_tokens: 0,
        }
    }

    /// Create token usage with all fields including reasoning tokens.
    pub fn full(
        input: u64,
        output: u64,
        reasoning: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output + reasoning,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            reasoning_tokens: reasoning,
        }
    }

    /// Check if any tokens were used.
    pub fn is_empty(&self) -> bool {
        self.total_tokens == 0 && self.reasoning_tokens == 0
    }

    /// Check if caching was used.
    pub fn used_cache(&self) -> bool {
        self.cache_read_tokens > 0 || self.cache_write_tokens > 0
    }

    /// Check if reasoning tokens were used.
    pub fn used_reasoning(&self) -> bool {
        self.reasoning_tokens > 0
    }
}

impl fmt::Display for TokenUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let has_cache = self.used_cache();
        let has_reasoning = self.used_reasoning();

        write!(
            f,
            "{} tokens (in: {}, out: {}",
            self.total_tokens, self.input_tokens, self.output_tokens
        )?;

        if has_reasoning {
            write!(f, ", reasoning: {}", self.reasoning_tokens)?;
        }
        if has_cache {
            write!(
                f,
                ", cache: r{}/w{}",
                self.cache_read_tokens, self.cache_write_tokens
            )?;
        }

        write!(f, ")")
    }
}

/// Cost information for an AI generation.
///
/// Tracks the monetary cost of AI usage for budgeting and attribution.
/// Note: We use micro_usd for equality comparisons to avoid float comparison issues.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Cost {
    /// Cost in USD (as a float for sub-cent precision)
    pub usd: f64,
    /// Cost in the smallest currency unit (e.g., cents for USD)
    /// Stored as integer for precise arithmetic
    #[serde(default)]
    pub micro_usd: u64,
}

impl Cost {
    /// Create a cost from USD amount.
    pub fn from_usd(usd: f64) -> Self {
        Self {
            usd,
            micro_usd: (usd * 1_000_000.0) as u64,
        }
    }

    /// Create a cost from micro-USD (millionths of a dollar).
    pub fn from_micro_usd(micro_usd: u64) -> Self {
        Self {
            usd: micro_usd as f64 / 1_000_000.0,
            micro_usd,
        }
    }

    /// Create zero cost.
    pub fn zero() -> Self {
        Self {
            usd: 0.0,
            micro_usd: 0,
        }
    }

    /// Check if the cost is zero.
    pub fn is_zero(&self) -> bool {
        self.micro_usd == 0
    }

    /// Add two costs.
    pub fn add(&self, other: &Cost) -> Self {
        Cost::from_micro_usd(self.micro_usd + other.micro_usd)
    }
}

impl PartialEq for Cost {
    fn eq(&self, other: &Self) -> bool {
        self.micro_usd == other.micro_usd
    }
}

impl Eq for Cost {}

impl std::hash::Hash for Cost {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.micro_usd.hash(state);
    }
}

impl fmt::Display for Cost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.usd < 0.01 {
            write!(f, "${:.4}", self.usd)
        } else {
            write!(f, "${:.2}", self.usd)
        }
    }
}

/// The prompt content, either hashed or full.
///
/// For privacy, prompts can be stored as just a hash. For full
/// auditability, the complete prompt can be stored (compressed).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptContent {
    /// Only the hash of the prompt is stored (for privacy)
    Hashed(Hash),
    /// The full prompt text is stored
    Full(String),
    /// The prompt is stored compressed (for large prompts)
    Compressed(Vec<u8>),
    /// No prompt information available
    #[default]
    None,
}

impl PromptContent {
    /// Create a hashed prompt from text.
    pub fn hash_from(prompt: &str) -> Self {
        PromptContent::Hashed(Hash::of(prompt.as_bytes()))
    }

    /// Create a full prompt.
    pub fn full(prompt: impl Into<String>) -> Self {
        PromptContent::Full(prompt.into())
    }

    /// Create from a pre-computed hash.
    pub fn from_hash(hash: Hash) -> Self {
        PromptContent::Hashed(hash)
    }

    /// Get the hash of this prompt (computes if full text is available).
    pub fn hash(&self) -> Option<Hash> {
        match self {
            PromptContent::Hashed(h) => Some(*h),
            PromptContent::Full(text) => Some(Hash::of(text.as_bytes())),
            PromptContent::Compressed(data) => Some(Hash::of(data)),
            PromptContent::None => None,
        }
    }

    /// Get the full text if available.
    pub fn text(&self) -> Option<&str> {
        match self {
            PromptContent::Full(text) => Some(text),
            _ => None,
        }
    }

    /// Check if this contains the full prompt text.
    pub fn has_full_text(&self) -> bool {
        matches!(self, PromptContent::Full(_))
    }

    /// Check if any prompt information is available.
    pub fn is_available(&self) -> bool {
        !matches!(self, PromptContent::None)
    }
}

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

/// Builder for constructing `Provenance` instances.
#[derive(Clone, Debug, Default)]
pub struct ProvenanceBuilder {
    vendor: Option<AIVendor>,
    model: Option<String>,
    model_version: Option<String>,
    tool: Option<AITool>,
    suggestion_type: SuggestionType,
    prompt: PromptContent,
    system_prompt_hash: Option<Hash>,
    tokens: TokenUsage,
    cost: Cost,
    temperature: Option<u32>,
    timestamp: Option<i64>,
    request_id: Option<String>,
    session_id: Option<String>,
    metadata: Vec<(String, String)>,
    agent_mode: Option<String>,
    finish_reason: Option<String>,
    step_count: Option<u32>,
    session_slug: Option<String>,
    reasoning_signature: Option<String>,
    reasoning_text: Option<String>,
    task_plan: Option<String>,
}

impl ProvenanceBuilder {
    /// Set the AI vendor.
    pub fn vendor(mut self, vendor: AIVendor) -> Self {
        self.vendor = Some(vendor);
        self
    }

    /// Set the vendor from a string.
    pub fn vendor_str(mut self, vendor: &str) -> Self {
        self.vendor = Some(AIVendor::parse(vendor));
        self
    }

    /// Set the model identifier.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set the model version.
    pub fn model_version(mut self, version: impl Into<String>) -> Self {
        self.model_version = Some(version.into());
        self
    }

    /// Set the tool used.
    pub fn tool(mut self, tool: AITool) -> Self {
        self.tool = Some(tool);
        self
    }

    /// Set the suggestion type.
    pub fn suggestion_type(mut self, suggestion_type: SuggestionType) -> Self {
        self.suggestion_type = suggestion_type;
        self
    }

    /// Set the prompt hash (for privacy).
    pub fn prompt_hash(mut self, hash: impl AsRef<str>) -> Self {
        if let Some(h) = Hash::from_base32(hash.as_ref().as_bytes()) {
            self.prompt = PromptContent::Hashed(h);
        }
        self
    }

    /// Set the prompt from a Hash.
    pub fn prompt_hash_value(mut self, hash: Hash) -> Self {
        self.prompt = PromptContent::Hashed(hash);
        self
    }

    /// Set the full prompt text.
    pub fn prompt_full(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = PromptContent::Full(prompt.into());
        self
    }

    /// Hash and set the prompt (for privacy).
    pub fn prompt_hashed_from(mut self, prompt: &str) -> Self {
        self.prompt = PromptContent::hash_from(prompt);
        self
    }

    /// Set the system prompt hash.
    pub fn system_prompt_hash(mut self, hash: Hash) -> Self {
        self.system_prompt_hash = Some(hash);
        self
    }

    /// Set input tokens.
    pub fn input_tokens(mut self, tokens: u64) -> Self {
        self.tokens.input_tokens = tokens;
        self.tokens.total_tokens = self.tokens.input_tokens + self.tokens.output_tokens;
        self
    }

    /// Set output tokens.
    pub fn output_tokens(mut self, tokens: u64) -> Self {
        self.tokens.output_tokens = tokens;
        self.tokens.total_tokens = self.tokens.input_tokens + self.tokens.output_tokens;
        self
    }

    /// Set token usage.
    pub fn tokens(mut self, input: u64, output: u64) -> Self {
        self.tokens = TokenUsage::new(input, output);
        self
    }

    /// Set cost in USD.
    pub fn cost_usd(mut self, usd: f64) -> Self {
        self.cost = Cost::from_usd(usd);
        self
    }

    /// Set temperature (0.0 to 2.0 range, stored as fixed-point).
    pub fn temperature(mut self, temp: f32) -> Self {
        self.temperature = Some((temp * 1000.0) as u32);
        self
    }

    /// Set timestamp (Unix epoch seconds).
    pub fn timestamp(mut self, ts: i64) -> Self {
        self.timestamp = Some(ts);
        self
    }

    /// Set request ID.
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }

    /// Set session ID.
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// Add metadata key-value pair.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.push((key.into(), value.into()));
        self
    }

    /// Set the agent mode (e.g., "build", "code", "ask").
    pub fn agent_mode(mut self, mode: impl Into<String>) -> Self {
        self.agent_mode = Some(mode.into());
        self
    }

    /// Set the finish reason (e.g., "stop", "tool-calls", "length").
    pub fn finish_reason(mut self, reason: impl Into<String>) -> Self {
        self.finish_reason = Some(reason.into());
        self
    }

    /// Set the number of LLM steps in the turn.
    pub fn step_count(mut self, count: u32) -> Self {
        self.step_count = Some(count);
        self
    }

    /// Set the human-readable session slug.
    pub fn session_slug(mut self, slug: impl Into<String>) -> Self {
        self.session_slug = Some(slug.into());
        self
    }

    /// Set the reasoning signature (cryptographic proof from model provider).
    pub fn reasoning_signature(mut self, sig: impl Into<String>) -> Self {
        self.reasoning_signature = Some(sig.into());
        self
    }

    /// Set the concatenated reasoning text from all thinking blocks.
    pub fn reasoning_text(mut self, text: impl Into<String>) -> Self {
        self.reasoning_text = Some(text.into());
        self
    }

    /// Set the agent's task plan (JSON string of todo items).
    pub fn task_plan(mut self, plan: impl Into<String>) -> Self {
        self.task_plan = Some(plan.into());
        self
    }

    /// Set reasoning tokens.
    pub fn reasoning_tokens(mut self, tokens: u64) -> Self {
        self.tokens.reasoning_tokens = tokens;
        self.tokens.total_tokens = self.tokens.input_tokens + self.tokens.output_tokens + tokens;
        self
    }

    /// Build the provenance.
    ///
    /// # Panics
    ///
    /// Panics if vendor or model is not set.
    pub fn build(self) -> Provenance {
        Provenance {
            vendor: self.vendor.unwrap_or_default(),
            model: self.model.unwrap_or_default(),
            model_version: self.model_version,
            tool: self.tool.unwrap_or_default(),
            suggestion_type: self.suggestion_type,
            prompt: self.prompt,
            system_prompt_hash: self.system_prompt_hash,
            tokens: self.tokens,
            cost: self.cost,
            temperature: self.temperature,
            timestamp: self.timestamp,
            request_id: self.request_id,
            session_id: self.session_id,
            metadata: self.metadata,
            agent_mode: self.agent_mode,
            finish_reason: self.finish_reason,
            step_count: self.step_count,
            session_slug: self.session_slug,
            reasoning_signature: self.reasoning_signature,
            reasoning_text: self.reasoning_text,
            task_plan: self.task_plan,
        }
    }

    /// Try to build the provenance, returning an error if required fields are missing.
    pub fn try_build(self) -> Result<Provenance, &'static str> {
        if self.vendor.is_none() {
            return Err("Vendor is required");
        }
        if self.model.is_none() || self.model.as_ref().map(|m| m.is_empty()).unwrap_or(true) {
            return Err("Model is required");
        }

        Ok(self.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AIVendor Tests

    #[test]
    fn test_vendor_names() {
        assert_eq!(AIVendor::Anthropic.name(), "anthropic");
        assert_eq!(AIVendor::OpenAI.name(), "openai");
        assert_eq!(AIVendor::Google.name(), "google");
        assert_eq!(AIVendor::Local.name(), "local");
        assert_eq!(AIVendor::Other("custom".to_string()).name(), "custom");
    }

    #[test]
    fn test_vendor_from_str() {
        assert_eq!(AIVendor::parse("anthropic"), AIVendor::Anthropic);
        assert_eq!(AIVendor::parse("claude"), AIVendor::Anthropic);
        assert_eq!(AIVendor::parse("openai"), AIVendor::OpenAI);
        assert_eq!(AIVendor::parse("gpt"), AIVendor::OpenAI);
        assert_eq!(AIVendor::parse("ollama"), AIVendor::Local);
        assert_eq!(
            AIVendor::parse("custom-ai"),
            AIVendor::Other("custom-ai".to_string())
        );
    }

    #[test]
    fn test_vendor_display() {
        assert_eq!(format!("{}", AIVendor::Anthropic), "anthropic");
    }

    #[test]
    fn test_vendor_json_roundtrip() {
        let vendor = AIVendor::Anthropic;
        let json = serde_json::to_string(&vendor).unwrap();
        let parsed: AIVendor = serde_json::from_str(&json).unwrap();
        assert_eq!(vendor, parsed);
    }

    // AITool Tests

    #[test]
    fn test_tool_description() {
        assert_eq!(AITool::Api.description(), "API");
        assert_eq!(AITool::Chat.description(), "Chat");
        assert_eq!(
            AITool::Editor("vscode".to_string()).description(),
            "Editor: vscode"
        );
        assert_eq!(AITool::editor("zed").description(), "Editor: zed");
    }

    #[test]
    fn test_tool_constructors() {
        assert_eq!(AITool::editor("zed"), AITool::Editor("zed".to_string()));
        assert_eq!(AITool::cli("atomic"), AITool::Cli("atomic".to_string()));
        assert_eq!(
            AITool::ide_plugin("copilot"),
            AITool::IdePlugin("copilot".to_string())
        );
    }

    #[test]
    fn test_tool_json_roundtrip() {
        let tool = AITool::Editor("vscode".to_string());
        let json = serde_json::to_string(&tool).unwrap();
        let parsed: AITool = serde_json::from_str(&json).unwrap();
        assert_eq!(tool, parsed);
    }

    // SuggestionType Tests

    #[test]
    fn test_suggestion_type_description() {
        assert_eq!(
            SuggestionType::Complete.description(),
            "AI-generated (complete)"
        );
        assert_eq!(
            SuggestionType::Collaborative.description(),
            "Human-AI collaborative"
        );
    }

    #[test]
    fn test_suggestion_type_human_contribution() {
        assert!(SuggestionType::Complete.human_contribution_estimate() < 0.3);
        assert!(SuggestionType::Review.human_contribution_estimate() > 0.5);
    }

    #[test]
    fn test_suggestion_type_default() {
        assert_eq!(SuggestionType::default(), SuggestionType::Collaborative);
    }

    // TokenUsage Tests

    #[test]
    fn test_token_usage_new() {
        let usage = TokenUsage::new(1000, 500);
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.total_tokens, 1500);
        assert!(!usage.is_empty());
        assert!(!usage.used_cache());
    }

    #[test]
    fn test_token_usage_with_cache() {
        let usage = TokenUsage::with_cache(1000, 500, 200, 100);
        assert!(usage.used_cache());
        assert_eq!(usage.cache_read_tokens, 200);
        assert_eq!(usage.cache_write_tokens, 100);
    }

    #[test]
    fn test_token_usage_display() {
        let usage = TokenUsage::new(1000, 500);
        let display = format!("{}", usage);
        assert!(display.contains("1500"));
        assert!(display.contains("1000"));
        assert!(display.contains("500"));
    }

    #[test]
    fn test_token_usage_empty() {
        let usage = TokenUsage::default();
        assert!(usage.is_empty());
    }

    // Cost Tests

    #[test]
    fn test_cost_from_usd() {
        let cost = Cost::from_usd(0.015);
        assert!((cost.usd - 0.015).abs() < 0.0001);
        assert_eq!(cost.micro_usd, 15000);
    }

    #[test]
    fn test_cost_from_micro_usd() {
        let cost = Cost::from_micro_usd(15000);
        assert!((cost.usd - 0.015).abs() < 0.0001);
    }

    #[test]
    fn test_cost_zero() {
        let cost = Cost::zero();
        assert!(cost.is_zero());
        assert_eq!(cost.usd, 0.0);
    }

    #[test]
    fn test_cost_add() {
        let c1 = Cost::from_usd(0.01);
        let c2 = Cost::from_usd(0.02);
        let total = c1.add(&c2);
        assert!((total.usd - 0.03).abs() < 0.0001);
    }

    #[test]
    fn test_cost_display() {
        let small = Cost::from_usd(0.001);
        assert!(format!("{}", small).contains("0.001"));

        let larger = Cost::from_usd(1.50);
        assert!(format!("{}", larger).contains("1.50"));
    }

    // PromptContent Tests

    #[test]
    fn test_prompt_hash_from() {
        let prompt = "Write a function to sort an array";
        let content = PromptContent::hash_from(prompt);
        assert!(content.hash().is_some());
        assert!(!content.has_full_text());
        assert!(content.is_available());
    }

    #[test]
    fn test_prompt_full() {
        let prompt = "Write a function";
        let content = PromptContent::full(prompt);
        assert!(content.has_full_text());
        assert_eq!(content.text(), Some("Write a function"));
        assert!(content.hash().is_some());
    }

    #[test]
    fn test_prompt_none() {
        let content = PromptContent::None;
        assert!(!content.is_available());
        assert!(content.hash().is_none());
    }

    #[test]
    fn test_prompt_default() {
        assert_eq!(PromptContent::default(), PromptContent::None);
    }

    // Provenance Tests

    #[test]
    fn test_provenance_new() {
        let prov = Provenance::new(
            AIVendor::Anthropic,
            "claude-sonnet-4-20250514",
            AITool::editor("zed"),
        );

        assert_eq!(prov.vendor, AIVendor::Anthropic);
        assert_eq!(prov.model, "claude-sonnet-4-20250514");
        assert_eq!(prov.tool, AITool::Editor("zed".to_string()));
    }

    #[test]
    fn test_provenance_builder() {
        let prov = Provenance::builder()
            .vendor(AIVendor::OpenAI)
            .model("gpt-4o")
            .tool(AITool::Api)
            .suggestion_type(SuggestionType::Complete)
            .tokens(2000, 1000)
            .cost_usd(0.05)
            .temperature(0.7)
            .build();

        assert_eq!(prov.vendor, AIVendor::OpenAI);
        assert_eq!(prov.model, "gpt-4o");
        assert_eq!(prov.suggestion_type, SuggestionType::Complete);
        assert_eq!(prov.tokens.total_tokens, 3000);
        assert!(prov.has_cost());
        assert!(prov.has_tokens());
        assert_eq!(prov.temperature, Some(700)); // 0.7 * 1000
    }

    #[test]
    fn test_provenance_with_prompt() {
        let prov = Provenance::new(AIVendor::Anthropic, "claude", AITool::Chat)
            .with_prompt_hashed("Write some code");

        assert!(prov.prompt.hash().is_some());
        assert!(!prov.prompt.has_full_text());
    }

    #[test]
    fn test_provenance_with_full_prompt() {
        let prov = Provenance::new(AIVendor::Anthropic, "claude", AITool::Chat)
            .with_prompt_full("Write some code");

        assert!(prov.prompt.has_full_text());
        assert_eq!(prov.prompt.text(), Some("Write some code"));
    }

    #[test]
    fn test_provenance_summary() {
        let prov = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .model("claude-sonnet-4")
            .tool(AITool::editor("zed"))
            .suggestion_type(SuggestionType::Collaborative)
            .build();

        let summary = prov.summary();
        assert!(summary.contains("anthropic"));
        assert!(summary.contains("claude-sonnet-4"));
        assert!(summary.contains("zed"));
    }

    #[test]
    fn test_provenance_display() {
        let prov = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .model("claude")
            .tool(AITool::Api)
            .tokens(1000, 500)
            .cost_usd(0.02)
            .build();

        let display = format!("{}", prov);
        assert!(display.contains("1500 tokens"));
        assert!(display.contains("$0.02"));
    }

    #[test]
    fn test_provenance_try_build_success() {
        let result = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .model("claude")
            .try_build();

        assert!(result.is_ok());
    }

    #[test]
    fn test_provenance_try_build_missing_model() {
        let result = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .try_build();

        assert!(result.is_err());
    }

    #[test]
    fn test_provenance_json_roundtrip() {
        let prov = Provenance::builder()
            .vendor(AIVendor::Anthropic)
            .model("claude-sonnet-4-20250514")
            .tool(AITool::editor("vscode"))
            .suggestion_type(SuggestionType::Partial)
            .tokens(1500, 500)
            .cost_usd(0.015)
            .request_id("req_123")
            .metadata("key", "value")
            .build();

        let json = serde_json::to_string(&prov).unwrap();
        let parsed: Provenance = serde_json::from_str(&json).unwrap();

        assert_eq!(prov.vendor, parsed.vendor);
        assert_eq!(prov.model, parsed.model);
        assert_eq!(prov.tokens.total_tokens, parsed.tokens.total_tokens);
        assert_eq!(prov.request_id, parsed.request_id);
    }

    #[test]
    fn test_provenance_builder_metadata() {
        let prov = Provenance::builder()
            .vendor(AIVendor::Local)
            .model("llama3")
            .tool(AITool::cli("ollama"))
            .metadata("gpu", "rtx4090")
            .metadata("quantization", "q4_k_m")
            .build();

        assert!(prov
            .metadata
            .contains(&("gpu".to_string(), "rtx4090".to_string())));
        assert!(prov
            .metadata
            .contains(&("quantization".to_string(), "q4_k_m".to_string())));
    }

    #[test]
    fn test_provenance_builder_vendor_str() {
        let prov = Provenance::builder()
            .vendor_str("anthropic")
            .model("claude")
            .build();

        assert_eq!(prov.vendor, AIVendor::Anthropic);
    }

    // Edge Cases

    #[test]
    fn test_empty_provenance() {
        let prov = Provenance::default();
        assert!(!prov.has_cost());
        assert!(!prov.has_tokens());
        assert!(prov.model.is_empty());
    }

    #[test]
    fn test_large_token_counts() {
        let usage = TokenUsage::new(1_000_000, 500_000);
        assert_eq!(usage.total_tokens, 1_500_000);
    }

    #[test]
    fn test_very_small_cost() {
        let cost = Cost::from_usd(0.000001);
        assert!(!cost.is_zero());
        assert_eq!(cost.micro_usd, 1);
    }
}
