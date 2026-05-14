//! Provider-agnostic LLM tool-use system for Atomic VCS.
//!
//! Provides:
//! - [`ToolExecutor`] trait for defining tool implementations
//! - [`RepoToolExecutor`] with five built-in repository tools
//! - Agentic loop ([`run_tool_loop`]) that drives multi-turn tool use
//! - Provider adapters for Anthropic and OpenAI wire formats

use std::path::Path;

use log::debug;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{AiError, AiProvider, LlmProvider};

// ── Constants ───────────────────────────────────────────────────

/// Maximum bytes returned from a tool result before truncation.
const MAX_TOOL_RESULT_BYTES: usize = 50 * 1024;

/// Maximum chars kept in [`ToolTraceEntry::result_preview`].
const MAX_PREVIEW_CHARS: usize = 200;

/// Maximum bytes for `read_file` tool output.
const MAX_READ_FILE_BYTES: usize = 50 * 1024;

/// If a file exceeds this size and no line range was given, return a
/// structured outline instead of dumping the whole file.
const READ_FILE_PREVIEW_THRESHOLD: usize = 32 * 1024; // 32KB ≈ 8k tokens

// ── Types ───────────────────────────────────────────────────────

/// A tool that the LLM can invoke.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A request from the LLM to invoke a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// A message in a tool-aware conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum ToolMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

/// Why the LLM stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

/// Response from a tool-aware LLM call.
#[derive(Debug, Clone)]
pub struct ToolAwareResponse {
    pub message: ToolMessage,
    pub stop_reason: StopReason,
    pub model: String,
    pub usage: TokenUsage,
}

/// Token consumption for a single LLM call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens read from Anthropic's prompt cache (much cheaper).
    pub cache_read_tokens: u64,
    /// Tokens written to cache on this request.
    pub cache_creation_tokens: u64,
}

/// Configuration for the agentic loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub system_prompt: String,
    pub max_turns: u8,
    pub max_tokens: u32,
    /// Print live tool call output to stderr.
    pub verbose: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            max_turns: 5,
            max_tokens: 4096,
            verbose: false,
        }
    }
}

/// Final result of an agentic tool-use session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub answer: String,
    pub model: String,
    pub total_usage: TokenUsage,
    pub turns: u8,
    pub tool_trace: Vec<ToolTraceEntry>,
}

/// Record of a single tool invocation for observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTraceEntry {
    pub turn: u8,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub result_preview: String,
    pub is_error: bool,
}

// ── ToolExecutor trait ──────────────────────────────────────────

/// Synchronous tool executor. Implementations provide tool definitions and
/// execute tool calls against concrete backends (repository, filesystem, etc.).
pub trait ToolExecutor: Send + Sync {
    /// Return the set of tools this executor provides.
    fn tool_definitions(&self) -> Vec<ToolDefinition>;

    /// Execute a tool call and return the result as a string.
    /// Errors are represented as `Err(message)` and surfaced to the LLM
    /// as `ToolResult { is_error: true }`.
    fn execute(&self, call: &ToolCall) -> Result<String, String>;
}

// ── LlmProvider: tool-aware chat ────────────────────────────────

impl LlmProvider {
    /// Send a tool-aware chat request to the configured provider.
    pub async fn chat_with_tools(
        &self,
        messages: &[ToolMessage],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<ToolAwareResponse, AiError> {
        match self.provider {
            AiProvider::Anthropic => {
                self.chat_with_tools_anthropic(messages, tools, max_tokens)
                    .await
            }
            AiProvider::OpenAi => {
                self.chat_with_tools_openai(messages, tools, max_tokens)
                    .await
            }
            _ => Err(AiError::NoApiKey),
        }
    }

    // ── Anthropic ───────────────────────────────────────────────

    async fn chat_with_tools_anthropic(
        &self,
        messages: &[ToolMessage],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<ToolAwareResponse, AiError> {
        let url = format!("{}/messages", self.base_url);

        // Extract system prompt (Anthropic uses a top-level field).
        let system_text: String = messages
            .iter()
            .filter_map(|m| match m {
                ToolMessage::System { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        // Build message array (skip System messages).
        let api_messages = anthropic_messages(messages);

        // Build tools array.
        let api_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": api_messages,
        });

        if !system_text.is_empty() {
            // Use content block format with cache_control for prompt caching.
            // The system prompt + tools are static across turns — caching them
            // means turns 2+ pay ~0 for the system prompt.
            body["system"] = json!([
                {
                    "type": "text",
                    "text": system_text,
                    "cache_control": { "type": "ephemeral" }
                }
            ]);
        }
        if !api_tools.is_empty() {
            body["tools"] = json!(api_tools);
        }

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
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(AiError::ApiError { status, body: text });
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        // Parse response.
        let content = json["content"].as_array().cloned().unwrap_or_default();

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in &content {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        text_parts.push(t.to_string());
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCall {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        name: block["name"].as_str().unwrap_or("").to_string(),
                        arguments: block["input"].clone(),
                    });
                }
                _ => {}
            }
        }

        let stop_reason = match json["stop_reason"].as_str() {
            Some("tool_use") => StopReason::ToolUse,
            Some("end_turn") => StopReason::EndTurn,
            Some("max_tokens") => StopReason::MaxTokens,
            _ => StopReason::Other,
        };

        let usage = TokenUsage {
            input_tokens: json["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: json["usage"]["output_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: json["usage"]["cache_read_input_tokens"]
                .as_u64()
                .unwrap_or(0),
            cache_creation_tokens: json["usage"]["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(0),
        };

        Ok(ToolAwareResponse {
            message: ToolMessage::Assistant {
                content: text_parts.join(""),
                tool_calls,
            },
            stop_reason,
            model: json["model"].as_str().unwrap_or(&self.model).to_string(),
            usage,
        })
    }

    // ── OpenAI ──────────────────────────────────────────────────

    async fn chat_with_tools_openai(
        &self,
        messages: &[ToolMessage],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<ToolAwareResponse, AiError> {
        let url = format!("{}/chat/completions", self.base_url);

        let api_messages = openai_messages(messages);

        let api_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": api_messages,
        });

        if !api_tools.is_empty() {
            body["tools"] = json!(api_tools);
        }

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
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(AiError::ApiError { status, body: text });
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AiError::Http(e.to_string()))?;

        let choice = &json["choices"][0];
        let msg = &choice["message"];

        let content_text = msg["content"].as_str().unwrap_or("").to_string();

        let mut tool_calls = Vec::new();
        if let Some(tcs) = msg["tool_calls"].as_array() {
            for tc in tcs {
                let func = &tc["function"];
                let args_str = func["arguments"].as_str().unwrap_or("{}");
                let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                tool_calls.push(ToolCall {
                    id: tc["id"].as_str().unwrap_or("").to_string(),
                    name: func["name"].as_str().unwrap_or("").to_string(),
                    arguments: args,
                });
            }
        }

        let stop_reason = match choice["finish_reason"].as_str() {
            Some("tool_calls") => StopReason::ToolUse,
            Some("stop") => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::Other,
        };

        let usage = TokenUsage {
            input_tokens: json["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: json["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        Ok(ToolAwareResponse {
            message: ToolMessage::Assistant {
                content: content_text,
                tool_calls,
            },
            stop_reason,
            model: json["model"].as_str().unwrap_or(&self.model).to_string(),
            usage,
        })
    }
}

// ── Wire-format helpers ─────────────────────────────────────────

/// Convert [`ToolMessage`] list into Anthropic's messages array.
fn anthropic_messages(messages: &[ToolMessage]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    // Collect consecutive ToolResult messages into a single "user" turn.
    let mut pending_results: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg {
            ToolMessage::System { .. } => { /* handled separately */ }
            ToolMessage::User { content } => {
                flush_pending_results(&mut pending_results, &mut out);
                out.push(json!({ "role": "user", "content": content }));
            }
            ToolMessage::Assistant {
                content,
                tool_calls,
            } => {
                flush_pending_results(&mut pending_results, &mut out);
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                if !content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": content }));
                }
                for tc in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments,
                    }));
                }
                out.push(json!({ "role": "assistant", "content": blocks }));
            }
            ToolMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                pending_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": content,
                }));
            }
        }
    }
    flush_pending_results(&mut pending_results, &mut out);
    out
}

fn flush_pending_results(pending: &mut Vec<serde_json::Value>, out: &mut Vec<serde_json::Value>) {
    if !pending.is_empty() {
        out.push(json!({ "role": "user", "content": std::mem::take(pending) }));
    }
}

/// Convert [`ToolMessage`] list into OpenAI's messages array.
fn openai_messages(messages: &[ToolMessage]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for msg in messages {
        match msg {
            ToolMessage::System { content } => {
                out.push(json!({ "role": "system", "content": content }));
            }
            ToolMessage::User { content } => {
                out.push(json!({ "role": "user", "content": content }));
            }
            ToolMessage::Assistant {
                content,
                tool_calls,
            } => {
                if tool_calls.is_empty() {
                    out.push(json!({ "role": "assistant", "content": content }));
                } else {
                    let tcs: Vec<serde_json::Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": serde_json::to_string(&tc.arguments)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                }
                            })
                        })
                        .collect();
                    let mut m = json!({
                        "role": "assistant",
                        "tool_calls": tcs,
                    });
                    if !content.is_empty() {
                        m["content"] = json!(content);
                    }
                    out.push(m);
                }
            }
            ToolMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                }));
            }
        }
    }
    out
}

// ── Agentic loop ────────────────────────────────────────────────

/// Run the agentic tool-use loop. The LLM can invoke tools up to
/// `config.max_turns` times before a final text response is forced.
pub async fn run_tool_loop(
    provider: &LlmProvider,
    executor: &dyn ToolExecutor,
    query: &str,
    config: &AgentConfig,
) -> Result<AgentResult, AiError> {
    let tool_defs = executor.tool_definitions();

    let mut messages: Vec<ToolMessage> = Vec::new();
    if !config.system_prompt.is_empty() {
        messages.push(ToolMessage::System {
            content: config.system_prompt.clone(),
        });
    }
    messages.push(ToolMessage::User {
        content: query.to_string(),
    });

    let mut total_usage = TokenUsage::default();
    let mut tool_trace: Vec<ToolTraceEntry> = Vec::new();
    let mut turn: u8 = 0;
    let mut last_text = String::new(); // accumulate text across turns

    loop {
        turn += 1;
        let at_limit = turn > config.max_turns;

        // If we've exhausted turns, inject a prompt telling the LLM to
        // answer now with what it has, rather than removing tools (which
        // confuses the conversation flow after tool_use/tool_result pairs).
        if at_limit {
            messages.push(ToolMessage::User {
                content: "You have used all available tool turns. Please provide your \
                          answer now based on what you have found so far. Be concise \
                          and cite file paths and line numbers."
                    .to_string(),
            });
        }
        let tools_for_call = &tool_defs; // always provide tools
        let tokens_for_call = if at_limit {
            config.max_tokens.max(8192)
        } else {
            config.max_tokens
        };

        debug!(
            "tool loop turn {turn}/{} (tools={})",
            config.max_turns,
            tools_for_call.len()
        );

        let resp = provider
            .chat_with_tools(&messages, tools_for_call, tokens_for_call)
            .await?;

        total_usage.input_tokens += resp.usage.input_tokens;
        total_usage.output_tokens += resp.usage.output_tokens;
        total_usage.cache_read_tokens += resp.usage.cache_read_tokens;
        total_usage.cache_creation_tokens += resp.usage.cache_creation_tokens;

        // Extract tool calls and text from the assistant message.
        let (text, calls) = match &resp.message {
            ToolMessage::Assistant {
                content,
                tool_calls,
            } => (content.clone(), tool_calls.clone()),
            _ => (String::new(), Vec::new()),
        };

        // Keep the latest non-empty text — the LLM may produce text on
        // intermediate turns (thinking aloud) or only on the final turn.
        if !text.trim().is_empty() {
            last_text = text.clone();
        }

        // Append the assistant message to history.
        messages.push(resp.message);

        // If no tool calls or we've hit the limit, we're done.
        if calls.is_empty() || resp.stop_reason != StopReason::ToolUse || at_limit {
            // Use the current turn's text if non-empty, otherwise fall back
            // to the last non-empty text from a previous turn.
            let answer = if text.trim().is_empty() {
                last_text
            } else {
                text
            };
            return Ok(AgentResult {
                answer,
                model: resp.model,
                total_usage,
                turns: turn,
                tool_trace,
            });
        }

        // Execute each tool call and append results.
        for call in &calls {
            debug!("executing tool: {} args={}", call.name, call.arguments);

            if config.verbose {
                // Show the tool call as an atomic CLI command equivalent
                let cmd_preview = format_tool_as_command(&call.name, &call.arguments);
                eprint!(
                    "  \x1b[2m[turn {}]\x1b[0m \x1b[36m{}\x1b[0m",
                    turn, cmd_preview
                );
            }

            let (result_content, is_error) = match executor.execute(call) {
                Ok(output) => (truncate_result(&output), false),
                Err(err) => (truncate_result(&err), true),
            };

            if config.verbose {
                if is_error {
                    eprintln!(" \x1b[31m✗\x1b[0m");
                } else {
                    let chars = result_content.chars().count();
                    eprintln!(" \x1b[32m✓\x1b[0m \x1b[2m({} chars)\x1b[0m", chars);
                }
            }

            tool_trace.push(ToolTraceEntry {
                turn,
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
                result_preview: preview(&result_content),
                is_error,
            });

            messages.push(ToolMessage::ToolResult {
                tool_call_id: call.id.clone(),
                content: result_content,
                is_error,
            });
        }
    }
}

/// Synchronous wrapper around [`run_tool_loop`].
pub fn run_tool_loop_sync(
    provider: &LlmProvider,
    executor: &dyn ToolExecutor,
    query: &str,
    config: &AgentConfig,
) -> Result<AgentResult, AiError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| AiError::Runtime(e.to_string()))?;
    rt.block_on(run_tool_loop(provider, executor, query, config))
}

// ── RepoToolExecutor ────────────────────────────────────────────

/// Tool executor backed by a [`crate::Repository`], providing knowledge-graph
/// search, file reading, entity extraction, and vault access.
pub struct RepoToolExecutor<'a> {
    repo: &'a crate::Repository,
    root: &'a Path,
}

impl<'a> RepoToolExecutor<'a> {
    pub fn new(repo: &'a crate::Repository) -> Self {
        Self {
            repo,
            root: repo.root(),
        }
    }
}

impl<'a> ToolExecutor for RepoToolExecutor<'a> {
    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        #[allow(unused_mut)]
        let mut defs = vec![
            ToolDefinition {
                name: "kg_search".into(),
                description: "Search the repository knowledge graph for nodes matching a \
                              query. Returns files, functions, changes, views, goals, intents."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Free-text search query"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results to return (default 10)"
                        }
                    },
                    "required": ["query"]
                }),
            },
            ToolDefinition {
                name: "kg_neighbors".into(),
                description: "Explore the knowledge-graph neighborhood around a node. \
                              Returns connected nodes and edges."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "node_id": {
                            "type": "string",
                            "description": "Node ID (e.g. \"file:src/main.rs\", \"change:abc123\")"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Traversal depth (default 1, max 2)"
                        }
                    },
                    "required": ["node_id"]
                }),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read a file from the working copy. For files over 8KB, \
                              returns a 50-line preview unless you specify start_line/end_line. \
                              Always use list_entities or code_search first to find the \
                              right line range instead of reading entire large files."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path from repository root"
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "First line to include (1-based, optional)"
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "Last line to include (1-based, inclusive, optional)"
                        }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "code_search".into(),
                description: "Search the source code content for a pattern. Returns matching \
                              lines with file paths and line numbers. Supports regex, path \
                              filtering, and file type filtering. Requires a content index \
                              (built by 'atomic vault query enrich')."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Search pattern (regex supported)"
                        },
                        "path_filter": {
                            "type": "string",
                            "description": "Restrict to files matching this path pattern (e.g. 'src/repl/')"
                        },
                        "file_type": {
                            "type": "string",
                            "description": "Restrict to this file type (e.g. 'rs', 'cpp', 'py')"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum results to return (default 30)"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDefinition {
                name: "vault_read".into(),
                description: "Read a vault entry (goal, intent, session, memory) by path.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Vault-relative path (e.g. \"goals/my-goal/_goal.md\")"
                        }
                    },
                    "required": ["path"]
                }),
            },
        ];

        #[cfg(feature = "ast")]
        defs.push(ToolDefinition {
            name: "list_entities".into(),
            description: "List code entities (functions, classes, types, etc.) in a file \
                          using tree-sitter AST extraction."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path from repository root"
                    }
                },
                "required": ["path"]
            }),
        });

        defs
    }

    fn execute(&self, call: &ToolCall) -> Result<String, String> {
        match call.name.as_str() {
            "kg_search" => self.exec_kg_search(&call.arguments),
            "kg_neighbors" => self.exec_kg_neighbors(&call.arguments),
            "read_file" => self.exec_read_file(&call.arguments),
            "code_search" => self.exec_code_search(&call.arguments),
            "vault_read" => self.exec_vault_read(&call.arguments),
            #[cfg(feature = "ast")]
            "list_entities" => self.exec_list_entities(&call.arguments),
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

impl<'a> RepoToolExecutor<'a> {
    fn exec_kg_search(&self, args: &serde_json::Value) -> Result<String, String> {
        let query = args["query"]
            .as_str()
            .ok_or("missing required parameter: query")?;
        let limit = args["limit"].as_u64().unwrap_or(10) as usize;

        let nodes = self
            .repo
            .vault_kg_search(query, limit, None)
            .map_err(|e| format!("kg_search failed: {e}"))?;

        serde_json::to_string_pretty(&nodes).map_err(|e| format!("serialize error: {e}"))
    }

    fn exec_kg_neighbors(&self, args: &serde_json::Value) -> Result<String, String> {
        let node_id = args["node_id"]
            .as_str()
            .ok_or("missing required parameter: node_id")?;
        let depth = args["depth"].as_u64().unwrap_or(1).min(2) as u8;

        let subgraph = self
            .repo
            .vault_kg_neighbors(node_id, depth)
            .map_err(|e| format!("kg_neighbors failed: {e}"))?;

        serde_json::to_string_pretty(&subgraph).map_err(|e| format!("serialize error: {e}"))
    }

    fn exec_read_file(&self, args: &serde_json::Value) -> Result<String, String> {
        let path_str = args["path"]
            .as_str()
            .ok_or("missing required parameter: path")?;

        // Prevent path traversal.
        if path_str.contains("..") {
            return Err("path must not contain '..'".into());
        }

        let full_path = self.root.join(path_str);
        let content = std::fs::read_to_string(&full_path)
            .map_err(|e| format!("cannot read {path_str}: {e}"))?;

        // Optionally slice by line range.
        let start_line = args["start_line"].as_u64().map(|n| n as usize);
        let end_line = args["end_line"].as_u64().map(|n| n as usize);

        let has_line_range = start_line.is_some() || end_line.is_some();
        let total_lines = content.lines().count();

        // If no line range and file is large, return a structured outline.
        // The LLM should use list_entities or code_search to find the
        // right line range first.
        if !has_line_range && content.len() > READ_FILE_PREVIEW_THRESHOLD {
            return Ok(build_file_outline(path_str, &content, total_lines));
        }

        let output = match (start_line, end_line) {
            (Some(s), Some(e)) => {
                let s = s.saturating_sub(1); // 1-based to 0-based
                content
                    .lines()
                    .skip(s)
                    .take(e.saturating_sub(s))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            (Some(s), None) => {
                let s = s.saturating_sub(1);
                content
                    .lines()
                    .skip(s)
                    .take(200) // cap open-ended reads at 200 lines
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            (None, Some(e)) => content.lines().take(e).collect::<Vec<_>>().join("\n"),
            (None, None) => content,
        };

        // Final cap on output size.
        if output.len() > MAX_READ_FILE_BYTES {
            Ok(format!(
                "{}\n\n... truncated ({} bytes total)",
                &output[..MAX_READ_FILE_BYTES],
                output.len()
            ))
        } else {
            Ok(output)
        }
    }

    fn exec_code_search(&self, args: &serde_json::Value) -> Result<String, String> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or("missing required parameter: pattern")?;

        let opts = crate::content_search::ContentSearchOptions {
            path_filter: args["path_filter"].as_str().map(String::from),
            file_type: args["file_type"].as_str().map(String::from),
            exclude_type: None,
            max_results: Some(args["max_results"].as_u64().unwrap_or(30) as usize),
            case_insensitive: false,
        };

        match crate::content_search::search_content(self.root, pattern, opts) {
            Ok(result) => {
                if result.matches.is_empty() {
                    Ok("No matches found.".into())
                } else {
                    // Build a compact response with matches + metadata
                    let response = json!({
                        "matches": result.matches,
                        "total_matches": result.total_matches,
                        "showing": result.matches.len(),
                        "dir_facets": result.dir_facets,
                    });
                    serde_json::to_string_pretty(&response)
                        .map_err(|e| format!("serialize error: {e}"))
                }
            }
            Err(crate::content_search::ContentSearchError::IndexNotFound) => {
                Err("Content index not built. Run 'atomic vault query enrich' first.".into())
            }
            Err(e) => Err(format!("code_search failed: {e}")),
        }
    }

    fn exec_vault_read(&self, args: &serde_json::Value) -> Result<String, String> {
        let path = args["path"]
            .as_str()
            .ok_or("missing required parameter: path")?;

        let entry = self
            .repo
            .vault_retrieve(path)
            .map_err(|e| format!("vault_read failed: {e}"))?;

        match entry {
            Some(e) => {
                let text = String::from_utf8_lossy(&e.content_bytes).into_owned();
                Ok(text)
            }
            None => Err(format!("vault entry not found: {path}")),
        }
    }

    #[cfg(feature = "ast")]
    fn exec_list_entities(&self, args: &serde_json::Value) -> Result<String, String> {
        let path_str = args["path"]
            .as_str()
            .ok_or("missing required parameter: path")?;

        if !atomic_semantic::is_supported(path_str) {
            return Err(format!("unsupported file type: {path_str}"));
        }

        let full_path = self.root.join(path_str);
        let source = std::fs::read_to_string(&full_path)
            .map_err(|e| format!("cannot read {path_str}: {e}"))?;

        let mut registry = atomic_semantic::ParserRegistry::new();
        let entities = registry.extract(path_str, &source);

        // Serialize a compact representation.
        let compact: Vec<serde_json::Value> = entities
            .iter()
            .map(|e| {
                let mut obj = json!({
                    "name": e.name,
                    "kind": e.kind.as_str(),
                    "line": e.line,
                    "end_line": e.end_line,
                    "exported": e.exported,
                });
                if let Some(sig) = &e.signature {
                    obj["signature"] = json!(sig);
                }
                obj
            })
            .collect();

        serde_json::to_string_pretty(&compact).map_err(|e| format!("serialize error: {e}"))
    }
}

// ── Helpers ─────────────────────────────────────────────────────

/// Build a structured outline of a large file instead of returning all content.
///
/// For markdown: extracts headings with line numbers.
/// For code: extracts function/class signatures using simple heuristics.
/// Always includes the first 20 lines as context.
fn build_file_outline(path: &str, content: &str, total_lines: usize) -> String {
    let mut out = format!(
        "File: {} ({} lines, {} bytes)\n\
         Use read_file with start_line/end_line to read specific sections.\n\n",
        path,
        total_lines,
        content.len()
    );

    let is_markdown = path.ends_with(".md") || path.ends_with(".mdx");

    if is_markdown {
        // Extract headings with line numbers
        out.push_str("Outline (headings):\n");
        for (i, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                out.push_str(&format!("  L{}: {}\n", i + 1, trimmed));
            }
        }
    } else {
        // For code files, show the first 30 lines + any lines that look like
        // definitions (fn, class, struct, def, func, impl, etc.)
        out.push_str("First 20 lines:\n");
        for (i, line) in content.lines().take(20).enumerate() {
            out.push_str(&format!("  {:>4}: {}\n", i + 1, line));
        }
        out.push_str("\nDefinitions found:\n");
        let def_patterns = [
            "fn ",
            "pub fn ",
            "async fn ",
            "def ",
            "func ",
            "function ",
            "class ",
            "struct ",
            "enum ",
            "trait ",
            "impl ",
            "interface ",
            "type ",
            "const ",
            "#define ",
            "namespace ",
        ];
        for (i, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if def_patterns.iter().any(|p| trimmed.starts_with(p)) {
                let short = if trimmed.len() > 100 {
                    format!("{}...", &trimmed[..97])
                } else {
                    trimmed.to_string()
                };
                out.push_str(&format!("  L{}: {}\n", i + 1, short));
            }
        }
    }

    out
}

/// Format a tool call as the equivalent `atomic query` CLI command.
fn format_tool_as_command(name: &str, args: &serde_json::Value) -> String {
    match name {
        "kg_search" => {
            let query = args["query"].as_str().unwrap_or("?");
            let limit = args["limit"].as_u64().unwrap_or(10);
            format!("atomic query search \"{}\" -k {}", query, limit)
        }
        "kg_neighbors" => {
            let id = args["node_id"].as_str().unwrap_or("?");
            let depth = args["depth"].as_u64().unwrap_or(1);
            format!("atomic query neighbors \"{}\" -d {}", id, depth)
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("?");
            match (args["start_line"].as_u64(), args["end_line"].as_u64()) {
                (Some(s), Some(e)) => format!("read_file {} L{}-{}", path, s, e),
                (Some(s), None) => format!("read_file {} L{}+", path, s),
                _ => format!("read_file {}", path),
            }
        }
        "code_search" => {
            let pattern = args["pattern"].as_str().unwrap_or("?");
            let mut cmd = format!("atomic query code \"{}\"", pattern);
            if let Some(t) = args["file_type"].as_str() {
                cmd.push_str(&format!(" -t {}", t));
            }
            if let Some(g) = args["path_filter"].as_str() {
                cmd.push_str(&format!(" -g {}", g));
            }
            cmd
        }
        "list_entities" => {
            let path = args["path"].as_str().unwrap_or("?");
            format!("atomic query entities {}", path)
        }
        "vault_read" => {
            let path = args["path"].as_str().unwrap_or("?");
            format!("atomic vault show {}", path)
        }
        _ => format!("{}({})", name, args),
    }
}

/// Truncate a tool result to [`MAX_TOOL_RESULT_BYTES`].
fn truncate_result(s: &str) -> String {
    if s.len() <= MAX_TOOL_RESULT_BYTES {
        s.to_string()
    } else {
        let truncated = &s[..MAX_TOOL_RESULT_BYTES];
        format!("{truncated}\n\n... truncated ({} bytes total)", s.len())
    }
}

/// Generate a short preview for [`ToolTraceEntry`].
fn preview(s: &str) -> String {
    if s.chars().count() <= MAX_PREVIEW_CHARS {
        s.to_string()
    } else {
        s.chars().take(MAX_PREVIEW_CHARS).collect::<String>() + "…"
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_result_short() {
        let s = "hello";
        assert_eq!(truncate_result(s), "hello");
    }

    #[test]
    fn test_truncate_result_long() {
        let s = "x".repeat(MAX_TOOL_RESULT_BYTES + 100);
        let result = truncate_result(&s);
        assert!(result.contains("truncated"));
        assert!(result.len() < s.len() + 100);
    }

    #[test]
    fn test_preview_short() {
        assert_eq!(preview("hello"), "hello");
    }

    #[test]
    fn test_preview_long() {
        let s = "a".repeat(300);
        let p = preview(&s);
        assert!(p.chars().count() <= MAX_PREVIEW_CHARS + 1);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn test_agent_config_default() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.max_turns, 5);
        assert_eq!(cfg.max_tokens, 4096);
        assert!(cfg.system_prompt.is_empty());
    }

    #[test]
    fn test_anthropic_messages_basic() {
        let msgs = vec![
            ToolMessage::System {
                content: "sys".into(),
            },
            ToolMessage::User {
                content: "hi".into(),
            },
        ];
        let result = anthropic_messages(&msgs);
        // System is excluded from messages array.
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
    }

    #[test]
    fn test_anthropic_messages_tool_results_batched() {
        let msgs = vec![
            ToolMessage::User {
                content: "q".into(),
            },
            ToolMessage::Assistant {
                content: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "t1".into(),
                        name: "kg_search".into(),
                        arguments: json!({"query": "test"}),
                    },
                    ToolCall {
                        id: "t2".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "x.rs"}),
                    },
                ],
            },
            ToolMessage::ToolResult {
                tool_call_id: "t1".into(),
                content: "result1".into(),
                is_error: false,
            },
            ToolMessage::ToolResult {
                tool_call_id: "t2".into(),
                content: "result2".into(),
                is_error: false,
            },
        ];

        let result = anthropic_messages(&msgs);
        // user, assistant, user (batched tool results)
        assert_eq!(result.len(), 3);
        let batched = result[2]["content"].as_array().unwrap();
        assert_eq!(batched.len(), 2);
        assert_eq!(batched[0]["type"], "tool_result");
        assert_eq!(batched[1]["type"], "tool_result");
    }

    #[test]
    fn test_openai_messages_system() {
        let msgs = vec![ToolMessage::System {
            content: "sys prompt".into(),
        }];
        let result = openai_messages(&msgs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "system");
        assert_eq!(result[0]["content"], "sys prompt");
    }

    #[test]
    fn test_openai_messages_tool_results_separate() {
        let msgs = vec![
            ToolMessage::ToolResult {
                tool_call_id: "t1".into(),
                content: "r1".into(),
                is_error: false,
            },
            ToolMessage::ToolResult {
                tool_call_id: "t2".into(),
                content: "r2".into(),
                is_error: false,
            },
        ];
        let result = openai_messages(&msgs);
        // Each tool result is a separate message for OpenAI.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "tool");
        assert_eq!(result[1]["role"], "tool");
    }

    #[test]
    fn test_openai_assistant_tool_calls_arguments_serialized() {
        let msgs = vec![ToolMessage::Assistant {
            content: "thinking".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "kg_search".into(),
                arguments: json!({"query": "hello"}),
            }],
        }];
        let result = openai_messages(&msgs);
        assert_eq!(result.len(), 1);
        let tc = &result[0]["tool_calls"][0];
        // OpenAI expects arguments as a JSON string.
        let args_str = tc["function"]["arguments"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(parsed["query"], "hello");
    }

    #[test]
    fn test_tool_definition_serialize() {
        let td = ToolDefinition {
            name: "test".into(),
            description: "a test tool".into(),
            parameters: json!({"type": "object"}),
        };
        let json = serde_json::to_value(&td).unwrap();
        assert_eq!(json["name"], "test");
    }

    /// Dummy executor for testing the loop mechanics.
    struct EchoExecutor;

    impl ToolExecutor for EchoExecutor {
        fn tool_definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "echo".into(),
                description: "echo args".into(),
                parameters: json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
            }]
        }

        fn execute(&self, call: &ToolCall) -> Result<String, String> {
            Ok(format!("echo: {}", call.arguments))
        }
    }

    #[test]
    fn test_echo_executor() {
        let exec = EchoExecutor;
        let defs = exec.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");

        let call = ToolCall {
            id: "1".into(),
            name: "echo".into(),
            arguments: json!({"msg": "hello"}),
        };
        let result = exec.execute(&call).unwrap();
        assert!(result.contains("hello"));
    }
}
