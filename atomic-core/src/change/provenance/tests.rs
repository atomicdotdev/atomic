//! Tests for AI provenance tracking.

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
    assert_eq!(AIVendor::parse("xai"), AIVendor::XAI);
    assert_eq!(AIVendor::parse("grok"), AIVendor::XAI);
    assert_eq!(AIVendor::XAI.name(), "xai");
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
