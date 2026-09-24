//! Read-only compatibility shims for historical V3 section payloads.
//!
//! V3's file header version stayed at 1 while some postcard payload structs
//! changed shape. Postcard serializes structs positionally, so `serde(default)`
//! cannot recover a field whose historical wire type or position changed.

use crate::change::{
    AITool, AIVendor, Cost, PromptContent, Provenance, SuggestionType, TokenUsage,
};
use crate::Hash;
use serde::Deserialize;

/// Provenance layout emitted by the July 2026 writer that produced the
/// reachable `Y2MEJPZ5...` change.
///
/// Compared with the current [`Provenance`] layout, `model_version` was a
/// required `String`, and `agent_name` plus `agent_version` appeared between
/// `suggestion_type` and `prompt`. The remaining fields, including the nested
/// six-counter [`TokenUsage`] and two-field [`Cost`] layouts, match current V3.
#[derive(Deserialize)]
struct July2026Provenance {
    vendor: AIVendor,
    model: String,
    model_version: String,
    tool: AITool,
    suggestion_type: SuggestionType,
    agent_name: String,
    agent_version: Option<String>,
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

impl From<July2026Provenance> for Provenance {
    fn from(legacy: July2026Provenance) -> Self {
        let mut metadata = legacy.metadata;
        preserve_metadata(&mut metadata, "agent_name", legacy.agent_name);
        if let Some(agent_version) = legacy.agent_version {
            preserve_metadata(&mut metadata, "agent_version", agent_version);
        }

        Self {
            vendor: legacy.vendor,
            model: legacy.model,
            model_version: Some(legacy.model_version),
            tool: legacy.tool,
            suggestion_type: legacy.suggestion_type,
            prompt: legacy.prompt,
            system_prompt_hash: legacy.system_prompt_hash,
            tokens: legacy.tokens,
            cost: legacy.cost,
            temperature: legacy.temperature,
            timestamp: legacy.timestamp,
            request_id: legacy.request_id,
            session_id: legacy.session_id,
            metadata,
            agent_mode: legacy.agent_mode,
            finish_reason: legacy.finish_reason,
            step_count: legacy.step_count,
            session_slug: legacy.session_slug,
            reasoning_signature: legacy.reasoning_signature,
            reasoning_text: legacy.reasoning_text,
            task_plan: legacy.task_plan,
        }
    }
}

fn preserve_metadata(metadata: &mut Vec<(String, String)>, key: &str, value: String) {
    if !metadata
        .iter()
        .any(|(existing_key, existing_value)| existing_key == key && existing_value == &value)
    {
        metadata.push((key.to_owned(), value));
    }
}

/// Decode the historical July 2026 provenance layout into current values.
pub(super) fn deserialize_july_2026_provenance(
    bytes: &[u8],
) -> Result<Vec<Provenance>, postcard::Error> {
    let legacy: Vec<July2026Provenance> = postcard::from_bytes(bytes)?;
    Ok(legacy.into_iter().map(Provenance::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_bytes() -> Vec<u8> {
        let hex: String = include_str!("fixtures/july_2026_legacy_provenance.hex")
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        assert_eq!(hex.len() % 2, 0);
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn july_2026_fixture_requires_compatibility_layout() {
        let bytes = fixture_bytes();
        assert!(matches!(
            postcard::from_bytes::<Vec<Provenance>>(&bytes),
            Err(postcard::Error::DeserializeBadOption)
        ));

        let decoded = deserialize_july_2026_provenance(&bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        let provenance = &decoded[0];
        assert_eq!(provenance.vendor, AIVendor::Local);
        assert_eq!(provenance.model, "moonshotai");
        assert_eq!(provenance.model_version.as_deref(), Some("kimi-k3"));
        assert_eq!(provenance.tool, AITool::Api);
        assert_eq!(provenance.suggestion_type, SuggestionType::Review);
        assert_eq!(provenance.timestamp, Some(1_784_502_467));
        assert_eq!(
            provenance.session_id.as_deref(),
            Some("ses_08365450affeTbe2zvcG63oKEX")
        );
        assert!(provenance
            .metadata
            .contains(&("agent_name".to_owned(), "opencode".to_owned())));
    }

    #[test]
    fn read_section_uses_legacy_fallback() {
        let section = crate::change::format_v3::ReadSection {
            section_type: crate::change::format_v3::SectionType::Provenance,
            payload: fixture_bytes(),
            compressed_size: 0,
            content_chunk_info: None,
        };

        let decoded: Vec<Provenance> = section.deserialize().unwrap();
        assert_eq!(decoded[0].model_version.as_deref(), Some("kimi-k3"));
        assert!(decoded[0]
            .metadata
            .contains(&("agent_name".to_owned(), "opencode".to_owned())));
    }

    #[test]
    fn converted_fixture_uses_the_unchanged_current_wire_shape() {
        let decoded = deserialize_july_2026_provenance(&fixture_bytes()).unwrap();
        let current_bytes = postcard::to_allocvec(&decoded).unwrap();
        assert_ne!(current_bytes, fixture_bytes());
        assert_eq!(
            postcard::from_bytes::<Vec<Provenance>>(&current_bytes).unwrap(),
            decoded
        );
    }
}
