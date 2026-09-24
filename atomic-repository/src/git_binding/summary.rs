//! The Git-safe attestation summary (RFC 5.6, §8.6, §12.13).
//!
//! Binding trees carry exactly one summary object: a **separately signed**,
//! allowlisted projection of agent attestation metadata — model vendor/name,
//! token counts, cost, and session id. Everything else an attestation may
//! contain (notes, request ids, metadata pairs, prompts) is dropped here by
//! construction: the summary type has no field that could carry it.
//!
//! Adding a field to this summary is a privacy-relevant change: it must go
//! through an allowlist review, not be smuggled in via `serde(flatten)` or an
//! open map. Transcripts, prompts, decision graphs, and unhashed private
//! bodies travel only through the Atomic transport boundary.

use std::collections::BTreeMap;

use atomic_core::change::attestation::Attestation;
use atomic_identity::keypair::{KeyPair, PublicKey};
use atomic_identity::signing::{Signature, Signer};
use serde::{Deserialize, Serialize};

/// Magic prefix of signed attestation summary bytes: `GAS1`.
pub const ATTESTATION_SUMMARY_MAGIC: &[u8; 4] = b"GAS1";

/// Domain separator for the summary's own Ed25519 signature.
pub const ATTESTATION_SUMMARY_SIGN_DOMAIN: &[u8] = b"atomic:binding-attestation-summary:sign:v1\0";

/// Current summary codec version.
pub const ATTESTATION_SUMMARY_VERSION: u32 = 1;

/// Per-model token and cost totals, aggregated by (vendor, model).
///
/// The allowlist: exactly what the RFC summary permits. No prompt, no
/// transcript, no free-form notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryModelUsage {
    /// AI vendor (e.g. `anthropic`).
    pub vendor: String,
    /// Model identifier (e.g. `claude-sonnet-4-5`).
    pub model: String,
    /// Input/prompt tokens.
    pub input_tokens: u64,
    /// Output/completion tokens.
    pub output_tokens: u64,
    /// Tokens read from cache.
    pub cache_read_tokens: u64,
    /// Tokens written to cache.
    pub cache_write_tokens: u64,
    /// Total cost in USD for this model's usage.
    pub cost_usd: f64,
}

impl SummaryModelUsage {
    /// Total tokens across all categories for this model.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// The allowlisted attestation summary carried in a binding tree.
///
/// Aggregate of the attestation chain for the bound session, bucketed by
/// (vendor, model). Every field is public, numeric, or a short identifier;
/// there is intentionally no field for prose, bodies, or unhashed material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingAttestationSummary {
    /// Summary codec version.
    pub version: u32,
    /// The agent session the covered changes belong to.
    pub session_id: String,
    /// Per-(vendor, model) usage, in deterministic sorted order.
    pub models: Vec<SummaryModelUsage>,
    /// Total input tokens across models.
    pub total_input_tokens: u64,
    /// Total output tokens across models.
    pub total_output_tokens: u64,
    /// Total cache-read tokens across models.
    pub total_cache_read_tokens: u64,
    /// Total cache-write tokens across models.
    pub total_cache_write_tokens: u64,
    /// Total cost in USD across models.
    pub total_cost_usd: f64,
}

impl BindingAttestationSummary {
    /// Project attestations onto the allowlisted summary for one session.
    ///
    /// Only the allowlisted fields survive: token counts, costs, vendor and
    /// model names, and the session id. Attestations are aggregated by
    /// (vendor, model) in deterministic order.
    pub fn from_attestations(session_id: &str, attestations: &[Attestation]) -> Self {
        let mut buckets: BTreeMap<(String, String), SummaryModelUsage> = BTreeMap::new();
        let mut total_input_tokens = 0u64;
        let mut total_output_tokens = 0u64;
        let mut total_cache_read_tokens = 0u64;
        let mut total_cache_write_tokens = 0u64;
        let mut total_cost_usd = 0f64;

        for attestation in attestations {
            for usage in &attestation.models {
                let key = (attestation.agent.vendor.clone(), usage.model.clone());
                let entry = buckets.entry(key).or_insert_with(|| SummaryModelUsage {
                    vendor: attestation.agent.vendor.clone(),
                    model: usage.model.clone(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cost_usd: 0.0,
                });
                entry.input_tokens += usage.input_tokens;
                entry.output_tokens += usage.output_tokens;
                entry.cache_read_tokens += usage.cache_read_tokens;
                entry.cache_write_tokens += usage.cache_write_tokens;
                entry.cost_usd += usage.cost_usd;
            }
            total_input_tokens += attestation
                .models
                .iter()
                .map(|m| m.input_tokens)
                .sum::<u64>();
            total_output_tokens += attestation
                .models
                .iter()
                .map(|m| m.output_tokens)
                .sum::<u64>();
            total_cache_read_tokens += attestation
                .models
                .iter()
                .map(|m| m.cache_read_tokens)
                .sum::<u64>();
            total_cache_write_tokens += attestation
                .models
                .iter()
                .map(|m| m.cache_write_tokens)
                .sum::<u64>();
            total_cost_usd += attestation.cost_usd;
        }

        Self {
            version: ATTESTATION_SUMMARY_VERSION,
            session_id: session_id.to_string(),
            models: buckets.into_values().collect(),
            total_input_tokens,
            total_output_tokens,
            total_cache_read_tokens,
            total_cache_write_tokens,
            total_cost_usd,
        }
    }

    /// Canonical summary bytes: magic + deterministic encoding.
    pub fn canonical_summary_bytes(&self) -> Result<Vec<u8>, BindingAttestationError> {
        let encoded = postcard::to_allocvec(self)
            .map_err(|error| BindingAttestationError::Encode(error.to_string()))?;
        let mut bytes = Vec::with_capacity(ATTESTATION_SUMMARY_MAGIC.len() + encoded.len());
        bytes.extend_from_slice(ATTESTATION_SUMMARY_MAGIC);
        bytes.extend_from_slice(&encoded);
        Ok(bytes)
    }
}

/// A separately signed attestation summary (magic || summary || signature).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedAttestationSummary {
    summary_bytes: Vec<u8>,
    signature: [u8; 64],
}

impl SignedAttestationSummary {
    /// Sign a summary with an Ed25519 keypair.
    pub fn sign(
        summary: &BindingAttestationSummary,
        keypair: &KeyPair,
    ) -> Result<Self, BindingAttestationError> {
        let summary_bytes = summary.canonical_summary_bytes()?;
        let mut message =
            Vec::with_capacity(ATTESTATION_SUMMARY_SIGN_DOMAIN.len() + summary_bytes.len());
        message.extend_from_slice(ATTESTATION_SUMMARY_SIGN_DOMAIN);
        message.extend_from_slice(&summary_bytes);
        let signature = Signer::new(keypair).sign(&message);
        Ok(Self {
            summary_bytes,
            signature: *signature.as_bytes(),
        })
    }

    /// Decode and validate signed summary bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, BindingAttestationError> {
        const SIGNATURE_SIZE: usize = 64;
        let minimum = ATTESTATION_SUMMARY_MAGIC.len() + SIGNATURE_SIZE + 1;
        if bytes.len() < minimum {
            return Err(BindingAttestationError::Malformed {
                reason: format!("truncated summary: {} bytes", bytes.len()),
            });
        }
        if &bytes[..ATTESTATION_SUMMARY_MAGIC.len()] != ATTESTATION_SUMMARY_MAGIC {
            return Err(BindingAttestationError::Malformed {
                reason: "wrong magic".to_string(),
            });
        }
        let signature_start = bytes.len() - SIGNATURE_SIZE;
        let summary = postcard::from_bytes::<BindingAttestationSummary>(
            &bytes[ATTESTATION_SUMMARY_MAGIC.len()..signature_start],
        )
        .map_err(|error| BindingAttestationError::Malformed {
            reason: error.to_string(),
        })?;
        if summary.version != ATTESTATION_SUMMARY_VERSION {
            return Err(BindingAttestationError::UnsupportedVersion {
                version: summary.version,
                supported: ATTESTATION_SUMMARY_VERSION,
            });
        }
        let reencoded = summary.canonical_summary_bytes()?;
        if bytes.len() != reencoded.len() + SIGNATURE_SIZE
            || reencoded.as_slice() != &bytes[..reencoded.len()]
        {
            return Err(BindingAttestationError::Malformed {
                reason: "summary encoding is not canonical".to_string(),
            });
        }
        let signature = <[u8; 64]>::try_from(&bytes[signature_start..]).map_err(|_| {
            BindingAttestationError::Malformed {
                reason: "signature is not exactly 64 bytes".to_string(),
            }
        })?;
        Ok(Self {
            summary_bytes: bytes[..signature_start].to_vec(),
            signature,
        })
    }

    /// Decode the allowlisted summary fields.
    pub fn summary(&self) -> Result<BindingAttestationSummary, BindingAttestationError> {
        postcard::from_bytes::<BindingAttestationSummary>(
            &self.summary_bytes[ATTESTATION_SUMMARY_MAGIC.len()..],
        )
        .map_err(|error| BindingAttestationError::Malformed {
            reason: error.to_string(),
        })
    }

    /// Canonical signed summary bytes as they enter the Git tree.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.summary_bytes.len() + self.signature.len());
        bytes.extend_from_slice(&self.summary_bytes);
        bytes.extend_from_slice(&self.signature);
        bytes
    }

    /// Verify the summary's own signature against `key`.
    pub fn verify_signature(&self, key: &PublicKey) -> Result<(), BindingAttestationError> {
        let mut message =
            Vec::with_capacity(ATTESTATION_SUMMARY_SIGN_DOMAIN.len() + self.summary_bytes.len());
        message.extend_from_slice(ATTESTATION_SUMMARY_SIGN_DOMAIN);
        message.extend_from_slice(&self.summary_bytes);
        let signature = Signature::from_bytes(self.signature);
        signature
            .verify(&message, key)
            .map_err(|error| BindingAttestationError::Signature {
                reason: error.to_string(),
            })
    }
}

/// Errors for the attestation summary boundary.
#[derive(Debug, thiserror::Error)]
pub enum BindingAttestationError {
    #[error("cannot encode attestation summary: {0}")]
    Encode(String),
    #[error("attestation summary is malformed: {reason}")]
    Malformed { reason: String },
    #[error("attestation summary signature verification failed: {reason}")]
    Signature { reason: String },
    #[error("unsupported attestation summary version {version}; this build supports version {supported}")]
    UnsupportedVersion { version: u32, supported: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::attestation::{AttestAgent, ModelUsage};

    fn attestation(session: &str, vendor: &str, model: &str, tokens: u64) -> Attestation {
        let mut usage = ModelUsage::new(model);
        usage.input_tokens = tokens;
        usage.output_tokens = tokens / 2;
        usage.cost_usd = 0.25;
        let mut value = Attestation::builder(
            session,
            AttestAgent::new("claude-code", "Claude Code", vendor),
        )
        .cost_usd(0.25)
        .build();
        value.models = vec![usage];
        value.notes = Some("PRIVATE NOTES SENTINEL".to_string());
        value
    }

    #[test]
    fn summary_is_an_allowlisted_projection() {
        let attestations = vec![
            attestation("session-1", "anthropic", "claude-sonnet-4-5", 100),
            attestation("session-1", "anthropic", "claude-sonnet-4-5", 40),
        ];
        let summary = BindingAttestationSummary::from_attestations("session-1", &attestations);
        assert_eq!(summary.models.len(), 1);
        let usage = &summary.models[0];
        assert_eq!(usage.vendor, "anthropic");
        assert_eq!(usage.model, "claude-sonnet-4-5");
        assert_eq!(usage.input_tokens, 140);
        assert_eq!(usage.output_tokens, 70);
        assert!((usage.cost_usd - 0.5).abs() < 1e-9);
        assert_eq!(summary.total_input_tokens, 140);

        // The allowlist drops everything the RFC does not name: the private
        // notes above (and any other attestation body) must not appear in the
        // canonical summary bytes, and the summary type carries no notes
        // field at all.
        let bytes = summary.canonical_summary_bytes().expect("canonical bytes");
        let rendered = String::from_utf8_lossy(&bytes);
        assert!(
            !rendered.contains("PRIVATE NOTES SENTINEL"),
            "summary bytes leaked an attestation note: {rendered}"
        );
        let decoded = postcard::from_bytes::<BindingAttestationSummary>(&bytes[4..]).unwrap();
        assert_eq!(decoded, summary);
    }
}
