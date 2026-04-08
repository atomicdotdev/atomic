//! Builder for constructing [`Provenance`](super::Provenance) instances.

use crate::types::Base32;
use crate::Hash;

use super::types::{AITool, AIVendor, Cost, PromptContent, SuggestionType, TokenUsage};
use super::Provenance;

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
