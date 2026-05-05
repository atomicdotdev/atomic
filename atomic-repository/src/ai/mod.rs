//! AI provider resolution for embeddings and LLM.
//!
//! Submodules:
//! - [`tools`] — provider-agnostic tool-use types, agentic loop, and repository tool executor.
//!
//! Resolves the best available AI provider from environment variables:
//!
//! | Env var              | Embedding provider             | LLM provider     |
//! |---------------------|--------------------------------|------------------|
//! | `ANTHROPIC_API_KEY` | Voyage (voyage-3, 1024d)       | Claude Haiku     |
//! | `OPENAI_API_KEY`    | text-embedding-3-small (1536d) | GPT-4o-mini      |
//! | Neither             | hash_embed placeholder         | None (search only) |

pub mod tools;

use serde::{Deserialize, Serialize};

use atomic_core::pristine::vault::{KgEdge, KgNode};

/// Which AI provider is being used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiProvider {
    Anthropic,
    OpenAi,
    Local,
    None,
}

/// Resolved embedding configuration.
#[derive(Debug, Clone)]
pub struct EmbeddingProvider {
    pub provider: AiProvider,
    pub model: String,
    pub dimensions: usize,
    api_key: Option<String>,
}

/// Resolved LLM configuration.
#[derive(Debug, Clone)]
pub struct LlmProvider {
    pub provider: AiProvider,
    pub model: String,
    pub(crate) api_key: String,
    pub(crate) base_url: String,
}

/// Result of an LLM call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub answer: String,
    pub model: String,
    pub tokens_used: Option<u64>,
}

// ── Provider Resolution ─────────────────────────────────────────

/// Resolve the best available embedding provider from environment.
pub fn resolve_embedding_provider() -> EmbeddingProvider {
    // Check for Voyage (via dedicated Voyage key)
    if let Ok(key) = std::env::var("VOYAGE_API_KEY") {
        if !key.is_empty() {
            return EmbeddingProvider {
                provider: AiProvider::Anthropic,
                model: "voyage-3".to_string(),
                dimensions: 1024,
                api_key: Some(key),
            };
        }
    }

    // Check for OpenAI
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            return EmbeddingProvider {
                provider: AiProvider::OpenAi,
                model: "text-embedding-3-small".to_string(),
                dimensions: 1536,
                api_key: Some(key),
            };
        }
    }

    // Check for Anthropic (use Voyage via Anthropic key)
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            return EmbeddingProvider {
                provider: AiProvider::Anthropic,
                model: "voyage-3".to_string(),
                dimensions: 1024,
                api_key: Some(key),
            };
        }
    }

    // Fallback: local hash-based placeholder
    EmbeddingProvider {
        provider: AiProvider::Local,
        model: "hash-embed".to_string(),
        dimensions: 384,
        api_key: None,
    }
}

/// Resolve the best available LLM provider from environment.
pub fn resolve_llm_provider() -> Option<LlmProvider> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            return Some(LlmProvider {
                provider: AiProvider::Anthropic,
                model: std::env::var("LLM_MODEL")
                    .unwrap_or_else(|_| "claude-haiku-4-5".to_string()),
                api_key: key,
                base_url: std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string()),
            });
        }
    }

    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            return Some(LlmProvider {
                provider: AiProvider::OpenAi,
                model: std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
                api_key: key,
                base_url: std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            });
        }
    }

    None
}

// ── Embedding API Calls ─────────────────────────────────────────

impl EmbeddingProvider {
    /// Embed a batch of texts using the resolved provider.
    ///
    /// Returns one vector per input text.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        match self.provider {
            AiProvider::Anthropic => self.embed_voyage(texts).await,
            AiProvider::OpenAi => self.embed_openai(texts).await,
            AiProvider::Local | AiProvider::None => Ok(texts
                .iter()
                .map(|t| crate::hash_embed(t, self.dimensions))
                .collect()),
        }
    }

    /// Embed a single text.
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, AiError> {
        let results = self.embed(&[text.to_string()]).await?;
        results.into_iter().next().ok_or(AiError::EmptyResponse)
    }

    /// Synchronous embed using a throwaway tokio runtime (for use in non-async contexts).
    ///
    /// When called from within an existing Tokio runtime (e.g., from hook handlers
    /// that call `vault_store` → `vault_auto_index` → `embed_sync`), spawns a
    /// separate thread to avoid the "cannot start a runtime from within a runtime"
    /// panic.
    pub fn embed_sync(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        match self.provider {
            AiProvider::Local | AiProvider::None => Ok(texts
                .iter()
                .map(|t| crate::hash_embed(t, self.dimensions))
                .collect()),
            _ => {
                if tokio::runtime::Handle::try_current().is_ok() {
                    // Already inside an async runtime — run the API call on a
                    // separate thread with its own runtime to avoid nesting.
                    let provider = self.clone();
                    let texts = texts.to_vec();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| AiError::Runtime(e.to_string()))?;
                        rt.block_on(provider.embed(&texts))
                    })
                    .join()
                    .unwrap_or_else(|_| Err(AiError::Runtime("Embedding thread panicked".into())))
                } else {
                    // Not inside a runtime — create a throwaway one directly.
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| AiError::Runtime(e.to_string()))?;
                    rt.block_on(self.embed(texts))
                }
            }
        }
    }

    async fn embed_voyage(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        let api_key = self.api_key.as_ref().ok_or(AiError::NoApiKey)?;

        let body = serde_json::json!({
            "model": self.model,
            "input": texts,
            "input_type": "document"
        });

        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.voyageai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AiError::ApiError {
                status: status.as_u16(),
                body: text,
            });
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        parse_embedding_response(&json)
    }

    async fn embed_openai(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        let api_key = self.api_key.as_ref().ok_or(AiError::NoApiKey)?;

        let body = serde_json::json!({
            "model": self.model,
            "input": texts,
        });

        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.openai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AiError::ApiError {
                status: status.as_u16(),
                body: text,
            });
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        parse_embedding_response(&json)
    }
}

/// Parse the standard embedding API response format (shared by OpenAI and Voyage).
fn parse_embedding_response(json: &serde_json::Value) -> Result<Vec<Vec<f32>>, AiError> {
    let data = json["data"]
        .as_array()
        .ok_or_else(|| AiError::ParseError("missing 'data' array".to_string()))?;

    let mut vectors = Vec::with_capacity(data.len());
    for item in data {
        let embedding = item["embedding"]
            .as_array()
            .ok_or_else(|| AiError::ParseError("missing 'embedding' array".to_string()))?;
        let vec: Vec<f32> = embedding
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        if vec.is_empty() {
            return Err(AiError::ParseError("empty embedding vector".to_string()));
        }
        vectors.push(vec);
    }
    Ok(vectors)
}

// ── LLM API Calls ───────────────────────────────────────────────

impl LlmProvider {
    /// Call the LLM to generate an answer from context.
    pub async fn answer(&self, context: &str, query: &str) -> Result<LlmResponse, AiError> {
        match self.provider {
            AiProvider::Anthropic => self.call_anthropic(context, query).await,
            AiProvider::OpenAi => self.call_openai(context, query).await,
            _ => Err(AiError::NoApiKey),
        }
    }

    /// Synchronous answer (for non-async contexts).
    pub fn answer_sync(&self, context: &str, query: &str) -> Result<LlmResponse, AiError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| AiError::Runtime(e.to_string()))?;
        rt.block_on(self.answer(context, query))
    }

    async fn call_anthropic(&self, context: &str, query: &str) -> Result<LlmResponse, AiError> {
        let url = format!("{}/messages", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 1024,
            "system": "You are a code assistant for a repository tracked by Atomic VCS (not git). \
                        The context includes: entities (functions, structs, traits extracted by AST), \
                        files, changes (these ARE the commit history — each has a date, sequence number, and message), \
                        views (like branches), goals (development sessions), and intents (work items). \
                        Change nodes show the project's history — use their dates and messages to answer questions about \
                        recent modifications. Answer using only the provided context. Be concise and precise.",
            "messages": [{
                "role": "user",
                "content": format!("{}\n\nQuestion: {}", context, query)
            }]
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AiError::ApiError {
                status: status.as_u16(),
                body: text,
            });
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        let text = json["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c["text"].as_str())
            .unwrap_or("")
            .to_string();

        let tokens = json["usage"]["output_tokens"].as_u64();

        Ok(LlmResponse {
            answer: text,
            model: self.model.clone(),
            tokens_used: tokens,
        })
    }

    async fn call_openai(&self, context: &str, query: &str) -> Result<LlmResponse, AiError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 1024,
            "messages": [
                {
                    "role": "system",
                    "content": "You are a code assistant for a repository tracked by Atomic VCS (not git). \
                                The context includes: entities (functions, structs, traits extracted by AST), \
                                files, changes (these ARE the commit history — each has a date, sequence number, and message), \
                                views (like branches), goals (development sessions), and intents (work items). \
                                Change nodes show the project's history — use their dates and messages to answer questions about \
                                recent modifications. Answer using only the provided context. Be concise and precise."
                },
                {
                    "role": "user",
                    "content": format!("{}\n\nQuestion: {}", context, query)
                }
            ]
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AiError::ApiError {
                status: status.as_u16(),
                body: text,
            });
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        let text = json["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c["message"]["content"].as_str())
            .unwrap_or("")
            .to_string();

        let tokens = json["usage"]["completion_tokens"].as_u64();

        Ok(LlmResponse {
            answer: text,
            model: self.model.clone(),
            tokens_used: tokens,
        })
    }
}

// ── Context Builder ─────────────────────────────────────────────

/// Build a compact text representation of KG nodes and edges for the LLM prompt.
///
/// Groups nodes by kind and renders edges as relationship sentences.
/// Hard cap at ~4000 characters to stay within context windows.
pub fn build_context_string(nodes: &[KgNode], edges: &[KgEdge], query: &str) -> String {
    const MAX_CHARS: usize = 4000;
    let mut buf = String::with_capacity(MAX_CHARS);
    buf.push_str(&format!("Repository context for query: \"{}\"\n\n", query));

    // Group by kind using a BTreeMap for deterministic output order
    let mut groups: std::collections::BTreeMap<&str, Vec<&KgNode>> =
        std::collections::BTreeMap::new();
    for node in nodes {
        groups.entry(node.kind.as_str()).or_default().push(node);
    }

    for (kind, group_nodes) in &groups {
        buf.push_str(&format!("{}:\n", capitalize(kind)));
        for node in group_nodes {
            buf.push_str(&format!("- {}", node.label));
            if let Some(ref summary) = node.summary {
                buf.push_str(&format!(": {}", summary));
            }
            // Render metadata for change nodes (timestamp, author, sequence)
            if node.kind == "change" {
                if let Some(ref meta) = node.metadata {
                    let mut details = Vec::new();
                    if let Some(ts) = meta.get("timestamp").and_then(|v| v.as_str()) {
                        details.push(format!("date: {}", ts));
                    }
                    if let Some(seq) = meta.get("sequence").and_then(|v| v.as_u64()) {
                        details.push(format!("#{}", seq));
                    }
                    if !details.is_empty() {
                        buf.push_str(&format!(" ({})", details.join(", ")));
                    }
                }
            }
            // Render metadata for entity nodes (kind, file, line range)
            if node.kind == "entity" {
                if let Some(ref meta) = node.metadata {
                    let mut details = Vec::new();
                    if let Some(k) = meta.get("kind").and_then(|v| v.as_str()) {
                        details.push(k.to_string());
                    }
                    if let Some(f) = meta.get("file").and_then(|v| v.as_str()) {
                        if let Some(line) = meta.get("line").and_then(|v| v.as_u64()) {
                            details.push(format!("{}:{}", f, line));
                        } else {
                            details.push(f.to_string());
                        }
                    }
                    if !details.is_empty() {
                        buf.push_str(&format!(" [{}]", details.join(", ")));
                    }
                }
            }
            buf.push('\n');
            if buf.len() >= MAX_CHARS {
                break;
            }
        }
        buf.push('\n');
        if buf.len() >= MAX_CHARS {
            break;
        }
    }

    if !edges.is_empty() {
        buf.push_str("Relationships:\n");
        for edge in edges {
            buf.push_str(&format!(
                "- {} -[{}]-> {}\n",
                edge.from_id, edge.kind, edge.to_id
            ));
            if buf.len() >= MAX_CHARS {
                break;
            }
        }
    }

    buf.truncate(MAX_CHARS);
    buf
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

// ── Errors ──────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("no API key configured")]
    NoApiKey,

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("API error (status {status}): {body}")]
    ApiError { status: u16, body: String },

    #[error("parse error: {0}")]
    ParseError(String),

    #[error("empty response")]
    EmptyResponse,

    #[error("runtime error: {0}")]
    Runtime(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_embedding_provider_default() {
        // When no env vars are set, should fall back to local.
        // (We can't reliably test this since env vars might be set in CI.)
        let provider = resolve_embedding_provider();
        // Just verify it returns something valid
        assert!(provider.dimensions > 0);
        assert!(!provider.model.is_empty());
    }

    #[test]
    fn test_resolve_llm_provider_none() {
        // Without env vars, should return None.
        // (Can't reliably test — env vars may be set.)
        // Just verify the function runs without panicking.
        let _ = resolve_llm_provider();
    }

    #[test]
    fn test_parse_embedding_response_openai_format() {
        let json = serde_json::json!({
            "data": [
                {"embedding": [0.1, 0.2, 0.3], "index": 0},
                {"embedding": [0.4, 0.5, 0.6], "index": 1}
            ]
        });
        let vectors = parse_embedding_response(&json).unwrap();
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0], vec![0.1_f32, 0.2, 0.3]);
        assert_eq!(vectors[1], vec![0.4_f32, 0.5, 0.6]);
    }

    #[test]
    fn test_parse_embedding_response_missing_data() {
        let json = serde_json::json!({});
        assert!(parse_embedding_response(&json).is_err());
    }

    #[test]
    fn test_parse_embedding_response_empty_vector() {
        let json = serde_json::json!({
            "data": [{"embedding": []}]
        });
        assert!(parse_embedding_response(&json).is_err());
    }

    #[test]
    fn test_build_context_string() {
        let nodes = vec![
            KgNode::new("change:abc", "change", "abc123", "graph").with_summary("Fix auth bug"),
            KgNode::new("file:src/auth.rs", "file", "src/auth.rs", "graph"),
        ];
        let edges = vec![KgEdge::new("change:abc", "file:src/auth.rs", "MODIFIES")];

        let context = build_context_string(&nodes, &edges, "who fixed auth?");
        assert!(context.contains("abc123"));
        assert!(context.contains("Fix auth bug"));
        assert!(context.contains("MODIFIES"));
        assert!(context.contains("who fixed auth?"));
    }

    #[test]
    fn test_build_context_string_empty() {
        let context = build_context_string(&[], &[], "test");
        assert!(context.contains("test"));
    }

    #[test]
    fn test_build_context_string_truncation() {
        // Create enough nodes to exceed MAX_CHARS
        let nodes: Vec<KgNode> = (0..500)
            .map(|i| {
                KgNode::new(
                    format!("entity:{}", i),
                    "entity",
                    format!("entity-{}", i),
                    "graph",
                )
                .with_summary(format!("A fairly long summary for entity number {} to help fill up the buffer quickly and test truncation behaviour", i))
            })
            .collect();
        let context = build_context_string(&nodes, &[], "big query");
        assert!(context.len() <= 4000);
    }

    #[test]
    fn test_build_context_string_groups_by_kind() {
        let nodes = vec![
            KgNode::new("file:a.rs", "file", "a.rs", "graph"),
            KgNode::new("change:x", "change", "x", "graph"),
            KgNode::new("file:b.rs", "file", "b.rs", "graph"),
        ];
        let context = build_context_string(&nodes, &[], "test");
        // BTreeMap orders keys: "change" < "file"
        let change_pos = context.find("Change:").expect("should contain Change:");
        let file_pos = context.find("File:").expect("should contain File:");
        assert!(
            change_pos < file_pos,
            "Change group should appear before File group (BTreeMap order)"
        );
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("change"), "Change");
        assert_eq!(capitalize("file"), "File");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("a"), "A");
    }

    #[test]
    fn test_local_embed_sync() {
        let provider = EmbeddingProvider {
            provider: AiProvider::Local,
            model: "hash-embed".to_string(),
            dimensions: 8,
            api_key: None,
        };
        let results = provider
            .embed_sync(&["hello".to_string(), "world".to_string()])
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].len(), 8);
        assert_eq!(results[1].len(), 8);
    }

    #[test]
    fn test_local_embed_sync_deterministic() {
        let provider = EmbeddingProvider {
            provider: AiProvider::Local,
            model: "hash-embed".to_string(),
            dimensions: 16,
            api_key: None,
        };
        let r1 = provider.embed_sync(&["deterministic".to_string()]).unwrap();
        let r2 = provider.embed_sync(&["deterministic".to_string()]).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_local_embed_sync_different_texts() {
        let provider = EmbeddingProvider {
            provider: AiProvider::Local,
            model: "hash-embed".to_string(),
            dimensions: 16,
            api_key: None,
        };
        let results = provider
            .embed_sync(&["alpha".to_string(), "beta".to_string()])
            .unwrap();
        assert_ne!(results[0], results[1]);
    }

    #[test]
    fn test_ai_provider_equality() {
        assert_eq!(AiProvider::Anthropic, AiProvider::Anthropic);
        assert_ne!(AiProvider::Anthropic, AiProvider::OpenAi);
        assert_ne!(AiProvider::Local, AiProvider::None);
    }

    #[test]
    fn test_parse_embedding_response_single_item() {
        let json = serde_json::json!({
            "data": [
                {"embedding": [1.0, 2.0], "index": 0}
            ]
        });
        let vectors = parse_embedding_response(&json).unwrap();
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0], vec![1.0_f32, 2.0]);
    }

    #[test]
    fn test_parse_embedding_response_missing_embedding_field() {
        let json = serde_json::json!({
            "data": [{"index": 0}]
        });
        assert!(parse_embedding_response(&json).is_err());
    }
}
