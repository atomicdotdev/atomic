//! Supporting types for AI provenance tracking.
//!
//! This module contains the enums and small structs used by [`Provenance`](super::Provenance):
//! vendor identifiers, tool categories, suggestion types, token usage,
//! cost tracking, and prompt content handling.

use crate::Hash;
use serde::{Deserialize, Serialize};
use std::fmt;

// ============================================================================
// AI VENDOR
// ============================================================================

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

// ============================================================================
// AI TOOL
// ============================================================================

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

// ============================================================================
// SUGGESTION TYPE
// ============================================================================

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

// ============================================================================
// TOKEN USAGE
// ============================================================================

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

// ============================================================================
// COST
// ============================================================================

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

// ============================================================================
// PROMPT CONTENT
// ============================================================================

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
