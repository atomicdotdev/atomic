//! Serializable provenance DTOs.
//!
//! Field names and skip rules mirror `atomic change -f json` so a server
//! response and the CLI's JSON output stay interchangeable.

use atomic_core::change::{PromptContent, Provenance};
use atomic_core::types::Base32;
use serde::{Deserialize, Serialize};

/// AI provenance attached to a change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceDto {
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
    pub tokens: Option<TokenUsageDto>,
    /// Cost information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostDto>,
    /// Temperature setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Request timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// Request ID from the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Session ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Hash of the prompt (base32), when only the hash is stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_hash: Option<String>,
    /// Additional metadata key/value pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Agent mode: "build", "code", "ask", …
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<String>,
    /// Why the model stopped on the final step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Number of LLM roundtrips (steps) in the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_count: Option<u32>,
    /// Human-readable session slug (e.g. "mighty-rocket").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_slug: Option<String>,
    /// Model-provider signature over the reasoning blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
    /// Concatenated reasoning/thinking text from the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_text: Option<String>,
    /// The agent's structured task plan at turn completion (JSON string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_plan: Option<String>,
}

/// Token usage counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsageDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Cost in micro-USD.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostDto {
    pub amount_micros: u64,
    pub currency: String,
}

impl From<&Provenance> for ProvenanceDto {
    fn from(prov: &Provenance) -> Self {
        let tokens = if prov.tokens.is_empty() {
            None
        } else {
            Some(TokenUsageDto {
                input: Some(prov.tokens.input_tokens),
                output: Some(prov.tokens.output_tokens),
                total: Some(prov.tokens.total_tokens),
            })
        };

        let cost = if prov.cost.is_zero() {
            None
        } else {
            Some(CostDto {
                amount_micros: prov.cost.micro_usd,
                currency: "USD".to_string(),
            })
        };

        let prompt_hash = match &prov.prompt {
            PromptContent::Hashed(h) => Some(h.to_base32()),
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
