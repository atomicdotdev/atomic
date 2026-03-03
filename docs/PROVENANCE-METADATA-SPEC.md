# Provenance Metadata Spec — Multi-Agent → Atomic Change

> **Status**: Active Design
> **Date**: February 2026
> **Source**: Runtime debug data from `opencode-atomic-hooks` plugin (`plugin-debug.log`)
>            and Anthropic Claude Code hooks reference documentation

## Overview

This document maps every piece of metadata available from supported agent hook
systems to the Atomic VCS provenance model. It covers:

- **OpenCode** — Plugin event bus with rich metadata (tokens, cost, reasoning
  blocks, structured diffs). Based on actual runtime data captured from the
  `plugin-debug.log` firehose.
- **Claude Code** — Anthropic's hooks system (`settings.json`) with lean
  per-event JSON plus a full conversation transcript (`transcript_path`).
  Based on the [Hooks reference](https://docs.anthropic.com/en/docs/claude-code/hooks).

The goal: every Atomic change recorded by an agent should carry a complete,
cryptographically signed, machine-readable provenance record that answers:

1. **Who** — which human prompted, which model responded, which tool orchestrated
2. **What** — exactly which files changed, with structured diffs
3. **Why** — the user's intent, the agent's reasoning, the task plan
4. **How** — which tool calls, in what order, with what results
5. **How much** — tokens, cost, duration, step count
6. **How trustworthy** — Anthropic signatures on reasoning, LSP diagnostics, build results

---

## Data Sources (from `plugin-debug.log`)

### Source 1: `chat.message` hook (fires per user prompt)

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| Session ID | `input.sessionID` | `"ses_36f721831ffe..."` | ✅ |
| User prompt | `output.parts[].text` | `"let's build a simple typescript hello world app"` | ✅ (fixed) |
| Agent mode | `input.agent` | `"build"`, `"code"`, `"ask"` | ✅ (new) |
| Model ID | `input.model.modelID` | `"claude-opus-4-5"`, `"claude-sonnet-4-5"` | ✅ |
| Provider ID | `input.model.providerID` | `"anthropic"` | ✅ |
| Message ID | `input.messageID` | `"msg_c908de7d4001..."` | ✅ (new, for threading) |
| Variant | `input.variant` | string or undefined | ❌ |

### Source 2: `message.updated` event — `AssistantMessage`

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| Model ID | `info.modelID` | `"claude-sonnet-4-5"` | ✅ |
| Provider ID | `info.providerID` | `"anthropic"` | ✅ |
| Agent mode | `info.mode` | `"build"` | ❌ |
| Total cost | `info.cost` | `0.07602125` | ❌ (only via step-finish) |
| Tokens (total) | `info.tokens.total` | `11639` | ❌ |
| Tokens (input) | `info.tokens.input` | `3` | ✅ (via step-finish) |
| Tokens (output) | `info.tokens.output` | `175` | ✅ (via step-finish) |
| Tokens (reasoning) | `info.tokens.reasoning` | `0` | ✅ (new) |
| Tokens (cache read) | `info.tokens.cache.read` | `0` | ✅ (via step-finish) |
| Tokens (cache write) | `info.tokens.cache.write` | `11461` | ✅ (via step-finish) |
| Finish reason | `info.finish` | `"tool-calls"`, `"stop"` | ❌ |
| Working directory | `info.path.cwd` | `"/Users/.../hello-world"` | ❌ |
| Parent message ID | `info.parentID` | `"msg_c908de7d4001..."` | ❌ |
| Time created | `info.time.created` | `1771951744991` | ❌ |
| Time completed | `info.time.completed` | `1771951748933` | ❌ |
| Error | `info.error` | `{ name, data }` or undefined | ❌ |
| Summary flag | `info.summary` | boolean | ❌ |

### Source 3: `step-finish` part (fires per LLM step within a turn)

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| Reason | `part.reason` | `"tool-calls"`, `"stop"` | ❌ |
| Cost | `part.cost` | `0.07602125` | ✅ |
| Tokens (total) | `part.tokens.total` | `11639` | ❌ |
| Tokens (input) | `part.tokens.input` | `3` | ✅ |
| Tokens (output) | `part.tokens.output` | `175` | ✅ |
| Tokens (reasoning) | `part.tokens.reasoning` | `0` | ✅ (new) |
| Tokens (cache read) | `part.tokens.cache.read` | `0` | ✅ |
| Tokens (cache write) | `part.tokens.cache.write` | `11461` | ✅ |
| Snapshot hash | `part.snapshot` | string or undefined | ❌ |
| Message ID | `part.messageID` | `"msg_c908de7df001..."` | ❌ |
| Step count | *(derived: count of step-finish per turn)* | 14 (turn 1), 6 (turn 2) | ❌ |

### Source 4: `reasoning` part (fires per thinking block)

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| Reasoning text | `part.text` | `"The user wants to build..."` (full chain-of-thought) | ✅ (debug only) |
| Text length | `part.text.length` | `578` | ✅ (debug only) |
| Time start | `part.time.start` | `1771953799126` (epoch ms) | ✅ (debug only) |
| Time end | `part.time.end` | `1771953801944` (epoch ms) | ✅ (debug only) |
| Duration | *(derived: end - start)* | `2818` ms | ❌ |
| Anthropic signature | `part.metadata.anthropic.signature` | base64 string (~500-1500 chars) | ✅ (debug only) |
| Provider metadata | `part.metadata` | `{ anthropic: { signature } }` | ✅ (debug only) |
| Message ID | `part.messageID` | `"msg_c90ad3a0b001..."` | ✅ (debug only) |

### Source 5: `tool.execute.before` hook

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| Tool name | `input.tool` | `"write"`, `"edit"`, `"bash"`, `"todowrite"` | ✅ |
| Session ID | `input.sessionID` | `"ses_36f721831ffe..."` | ✅ |
| Call ID | `input.callID` | `"toolu_015PXxuVtx7F..."` | ✅ |
| Args | `output.args` | `{ filePath, content }` or `{ command, description }` | ✅ (debug only) |

### Source 6: `tool.execute.after` hook (RICHEST DATA)

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| Tool name | `input.tool` | `"edit"` | ✅ |
| Session ID | `input.sessionID` | `"ses_36f721831ffe..."` | ✅ |
| Call ID | `input.callID` | `"toolu_01EFoT6mPt..."` | ✅ |
| Args (input) | `input.args` | `{ filePath, oldString, newString }` | ✅ (new) |
| Title | `output.title` | `"Users/.../package.json"` | ✅ (new) |
| Output text | `output.output` | `"Edit applied successfully."` | ✅ |
| **File diff** | `output.metadata.filediff` | `{ file, before, after, additions, deletions }` | ✅ (new) |
| Unified diff | `output.metadata.diff` | unified diff string | ✅ (new) |
| **Diagnostics** | `output.metadata.diagnostics` | `{ "/path/file.ts": [{ range, message, severity }] }` | ✅ (new) |
| File path | `output.metadata.filepath` | `"/Users/.../tsconfig.json"` | ✅ (new) |
| File existed | `output.metadata.exists` | `false` (new file) or `true` (edit) | ❌ |
| Truncated | `output.metadata.truncated` | `false` | ❌ |
| Exit code (bash) | `output.metadata.exit` | `0` | ✅ (new) |
| Command (bash) | `input.args.command` | `"npm init -y"` | ✅ (via args) |
| Description (bash) | `input.args.description` | `"Initialize npm project"` | ✅ (via args) |

### Source 7: `todo.updated` event (agent task plan)

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| Session ID | `properties.sessionID` | `"ses_36f721831ffe..."` | ❌ |
| Todos | `properties.todos[]` | `[{ content, status, priority }]` | ❌ |
| Todo content | `todos[].content` | `"Initialize npm project"` | ❌ |
| Todo status | `todos[].status` | `"in_progress"`, `"completed"`, `"pending"` | ❌ |
| Todo priority | `todos[].priority` | `"high"`, `"medium"`, `"low"` | ❌ |

### Source 8: `file.edited` and `file.watcher.updated` events

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| File path | `properties.file` | `"/Users/.../tsconfig.json"` | ❌ |
| Watcher event | `properties.event` | `"add"`, `"change"`, `"unlink"` | ❌ |

### Source 9: `session.created` / `session.updated` events

| Field | Path | Example | Currently Captured |
|-------|------|---------|-------------------|
| Session ID | `info.id` | `"ses_36f721831ffe..."` | ✅ |
| Session slug | `info.slug` | `"mighty-rocket"` | ❌ |
| Project ID | `info.projectID` | `"global"` | ❌ |
| Directory | `info.directory` | `"/Users/.../hello-world"` | ✅ (via cwd) |
| Title | `info.title` | `"Building hello world TypeScript app"` | ❌ |
| Version | `info.version` | `"0.0.0-dev-202602192030"` | ❌ |
| Summary additions | `info.summary.additions` | `42` | ❌ |
| Summary deletions | `info.summary.deletions` | `3` | ❌ |
| Summary files | `info.summary.files` | `4` | ❌ |
| Time created | `info.time.created` | `1771951744974` | ❌ |
| Time updated | `info.time.updated` | `1771951745845` | ❌ |

---

## Current Rust Provenance Model — Gaps

### `Provenance` struct (per-change, in `atomic-core/src/change/provenance.rs`)

| Field | Type | Status | Gap |
|-------|------|--------|-----|
| `vendor` | `AIVendor` | ✅ | — |
| `model` | `String` | ✅ | — |
| `model_version` | `Option<String>` | ⚠️ | Never populated from OpenCode |
| `tool` | `AITool` | ✅ | Hardcoded to `Cli("opencode")` |
| `suggestion_type` | `SuggestionType` | ⚠️ | Hardcoded to `Complete`, should derive from `agent` mode |
| `prompt` | `PromptContent` | ⚠️ | Hash only — should optionally store full text |
| `system_prompt_hash` | `Option<Hash>` | ❌ | Never populated |
| `tokens` | `TokenUsage` | ⚠️ | **Missing `reasoning_tokens`** |
| `cost` | `Cost` | ⚠️ | Never populated from stop payload |
| `temperature` | `Option<u32>` | ❌ | Not available from OpenCode |
| `timestamp` | `Option<i64>` | ✅ | — |
| `request_id` | `Option<String>` | ❌ | Not available from OpenCode |
| `session_id` | `Option<String>` | ✅ | — |
| `metadata` | `Vec<(String, String)>` | ⚠️ | Only `turn_number` and `agent_name` |

**Missing from struct entirely:**

| Needed Field | Source | Why |
|---|---|---|
| `agent_mode` | `chat.message` → `input.agent` | "build" vs "code" vs "ask" changes the accountability context |
| `reasoning_tokens` | `step-finish` → `tokens.reasoning` | Reasoning token billing (o1, o3, extended thinking) |
| `finish_reason` | `step-finish` → `reason` | "stop" = agent chose to finish, "tool-calls" = agent wanted more |
| `step_count` | derived from step-finish count | How many LLM roundtrips this turn required |
| `session_slug` | `session.created` → `info.slug` | Human-readable session name for display |
| `anthropic_signature` | `reasoning` → `metadata.anthropic.signature` | Cryptographic proof reasoning is genuine |

### `TokenUsage` struct (in `atomic-core/src/change/provenance.rs`)

| Field | Type | Status | Gap |
|-------|------|--------|-----|
| `input_tokens` | `u64` | ✅ | — |
| `output_tokens` | `u64` | ✅ | — |
| `total_tokens` | `u64` | ✅ | — |
| `cache_read_tokens` | `u64` | ✅ | — |
| `cache_write_tokens` | `u64` | ✅ | — |
| **`reasoning_tokens`** | — | ❌ | **Must add** — separate billing category for thinking models |

### `ProvenanceGraph` / `ProvenanceNode` (in `atomic-core/src/change/provenance_graph.rs`)

The graph model has the right *structure* for what we need (nodes with kinds,
causal edges, stats). But the data flowing into it from OpenCode is thin because
the plugin only sends tool names and call IDs via `after-tool`, not the rich
metadata.

**ProvenanceNode gaps:**

| Needed | Currently | Source |
|--------|-----------|--------|
| Structured file diff on Commitment nodes | Only `tool_name`, `tool_call_id` | `tool.execute.after` → `metadata.filediff` |
| LSP diagnostics on Verification nodes | Not captured | `tool.execute.after` → `metadata.diagnostics` |
| Bash exit code + output on Execution nodes | Not captured | `tool.execute.after` → `metadata.exit`, `output` |
| Reasoning text on Decision nodes | Not captured | `reasoning` parts → `text` |
| Reasoning duration on Decision nodes | Not captured | `reasoning` parts → `time.end - time.start` |
| Anthropic signature on Decision nodes | Not captured | `reasoning` parts → `metadata.anthropic.signature` |
| Todo plan on Goal nodes | Not captured | `todo.updated` → `todos[]` |

---

## Proposed Model Changes

### 1. `TokenUsage` — Add reasoning tokens

```rust
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    // NEW: reasoning/thinking tokens (separate billing for o1, o3, extended thinking)
    #[serde(default)]
    pub reasoning_tokens: u64,
}
```

**Migration**: `#[serde(default)]` means existing serialized data deserializes
with `reasoning_tokens: 0`. No migration needed.

### 2. `Provenance` — Add new fields

```rust
pub struct Provenance {
    // --- existing fields (unchanged) ---
    pub vendor: AIVendor,
    pub model: String,
    pub model_version: Option<String>,
    pub tool: AITool,
    pub suggestion_type: SuggestionType,
    pub prompt: PromptContent,
    pub system_prompt_hash: Option<Hash>,
    pub tokens: TokenUsage,                   // now includes reasoning_tokens
    pub cost: Cost,
    pub temperature: Option<u32>,
    pub timestamp: Option<i64>,
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub metadata: Vec<(String, String)>,

    // --- NEW fields ---

    /// Agent mode: "build", "code", "ask", etc.
    /// Determines the accountability context — "build" means the agent has
    /// full autonomy, "ask" means advisory only.
    #[serde(default)]
    pub agent_mode: Option<String>,

    /// Why the model stopped generating: "stop", "tool-calls", "length"
    /// "stop" = agent decided it was done.
    /// "tool-calls" = agent wanted to execute tools (multi-step turn).
    /// "length" = context window exhausted.
    #[serde(default)]
    pub finish_reason: Option<String>,

    /// Number of LLM roundtrips (steps) in this turn.
    /// Each step-finish event increments this counter.
    #[serde(default)]
    pub step_count: Option<u32>,

    /// Human-readable session slug (e.g., "mighty-rocket").
    /// Assigned by OpenCode, useful for display and correlation.
    #[serde(default)]
    pub session_slug: Option<String>,

    /// Cryptographic signature from the model provider on reasoning blocks.
    /// Currently Anthropic-specific: proves the chain-of-thought was genuinely
    /// produced by the model, not fabricated. Stored as the last complete
    /// reasoning block's signature.
    #[serde(default)]
    pub reasoning_signature: Option<String>,
}
```

**Migration**: All new fields are `Option` with `#[serde(default)]`.
Existing serialized changes deserialize cleanly.

### 3. `StopInput` (OpenCode hook) — Accept new fields from plugin

```rust
struct StopInput {
    // --- existing ---
    session_id: Option<String>,
    turn_number: Option<u32>,
    model: Option<String>,
    provider: Option<String>,
    error: Option<bool>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    cost_usd: Option<f64>,
    cwd: Option<String>,
    timestamp: Option<String>,

    // --- NEW ---
    /// Agent mode from chat.message hook
    agent: Option<String>,
    /// Reasoning tokens (thinking/chain-of-thought)
    reasoning_tokens: Option<u64>,
    /// Last finish reason from step-finish: "stop", "tool-calls"
    finish_reason: Option<String>,
    /// Number of LLM steps in this turn
    step_count: Option<u32>,
    /// Session slug from session.created
    session_slug: Option<String>,
    /// Concatenated reasoning text from all reasoning parts in this turn
    reasoning_text: Option<String>,
    /// Anthropic signature from the last reasoning block
    reasoning_signature: Option<String>,
    /// Agent's todo list at turn completion
    todos: Option<serde_json::Value>,
}
```

### 4. `AfterToolInput` (OpenCode hook) — Accept rich metadata from plugin

```rust
struct AfterToolInput {
    // --- existing ---
    session_id: Option<String>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    status: Option<String>,
    duration: Option<u64>,
    modified_files: Option<bool>,
    tool_output: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,

    // --- NEW ---
    /// Human-readable title (e.g., "Install TypeScript as dev dependency")
    title: Option<String>,
    /// Absolute file path for write/edit tools
    file_path: Option<String>,
    /// Structured file diff: { file, before, after, additions, deletions }
    filediff: Option<serde_json::Value>,
    /// LSP diagnostics at time of edit
    diagnostics: Option<serde_json::Value>,
    /// Exit code for bash tools
    exit_code: Option<i32>,
}
```

### 5. `UserPromptInput` (OpenCode hook) — Accept agent mode

```rust
struct UserPromptInput {
    // --- existing ---
    session_id: Option<String>,
    prompt: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,

    // --- NEW ---
    /// Agent mode: "build", "code", "ask"
    agent: Option<String>,
}
```

### 6. `ProvenanceNode` — Richer node detail

The `detail` field on `ProvenanceNode` is `Option<String>` (JSON string). For
each node kind, we define a structured detail schema:

**Commitment node detail (write/edit tools):**
```json
{
  "file_path": "/Users/.../tsconfig.json",
  "operation": "create",
  "additions": 14,
  "deletions": 0,
  "before_hash": null,
  "after_hash": "blake3:abc123...",
  "diagnostics": {
    "/Users/.../src/index.ts": [
      { "line": 1, "severity": "error", "message": "Cannot find module..." }
    ]
  }
}
```

**Decision node detail (reasoning blocks):**
```json
{
  "reasoning_text": "The user wants to build a simple TypeScript...",
  "reasoning_duration_ms": 2818,
  "reasoning_tokens": 142,
  "anthropic_signature": "EucFCkYICxgCKk...",
  "step_reason": "tool-calls"
}
```

**Execution node detail (bash tools):**
```json
{
  "command": "npm install typescript --save-dev",
  "description": "Install TypeScript as dev dependency",
  "exit_code": 0,
  "output_summary": "added 1 package, and audited 2 packages in 741ms",
  "duration_ms": 1200
}
```

**Goal node detail (user prompt + plan):**
```json
{
  "prompt": "let's build a simple typescript hello world app",
  "agent_mode": "build",
  "model": "claude-sonnet-4-5",
  "provider": "anthropic",
  "todos": [
    { "content": "Initialize npm project", "status": "pending", "priority": "high" },
    { "content": "Install TypeScript", "status": "pending", "priority": "high" }
  ]
}
```

**Verification node detail (test/lint/build results):**
```json
{
  "command": "npm run build",
  "exit_code": 0,
  "output_summary": "> tsc\n\n",
  "diagnostics_clean": true,
  "duration_ms": 2100
}
```

---

## Plugin → Rust Data Flow

### Per-Turn Accumulation (in TypeScript plugin session state)

The plugin accumulates data across the events within a single turn (from
`chat.message` to `session.idle`), then sends it all in the `stop` and
`after-tool` payloads:

```
chat.message
  → store: prompt, agent mode, model, provider, user messageID

message.part.updated (step-finish) × N
  → accumulate: tokens, cost per step
  → track: step count, last finish reason

message.part.updated (reasoning) × N
  → accumulate: reasoning text blocks, durations
  → capture: last Anthropic signature

message.part.updated (tool: running/completed) × N
  → track: pending → completed tool calls

tool.execute.before × N
  → store: tool args (filePath, content, command)

tool.execute.after × N
  → enrich: filediff, diagnostics, exit code, title

todo.updated × N
  → store: latest todo list

session.updated
  → store: session slug, title

session.idle (TURN BOUNDARY)
  → send after-tool for each completed tool (with rich metadata)
  → send stop with accumulated tokens, cost, reasoning, todos
  → reset per-turn state
```

### `stop` payload (sent to `atomic agent hooks opencode stop`)

```json
{
  "session_id": "ses_36f721831ffe...",
  "cwd": "/Users/.../hello-world",
  "timestamp": "2026-02-24T16:49:49.034Z",
  "turn_number": 1,
  "model": "claude-sonnet-4-5",
  "provider": "anthropic",
  "agent": "build",
  "input_tokens": 16,
  "output_tokens": 1805,
  "reasoning_tokens": 578,
  "cache_read_tokens": 167461,
  "cache_write_tokens": 14437,
  "cost_usd": 0.2192,
  "step_count": 14,
  "finish_reason": "stop",
  "session_slug": "mighty-rocket",
  "reasoning_text": "The user wants to build a simple TypeScript...\n---\nThe directory is relatively empty...\n---\n...",
  "reasoning_signature": "EucFCkYICxgCKk...",
  "todos": [
    { "content": "Initialize npm project", "status": "completed", "priority": "high" },
    { "content": "Create Hello World TypeScript file", "status": "completed", "priority": "high" }
  ]
}
```

### `after-tool` payload (sent per completed tool call)

```json
{
  "session_id": "ses_36f721831ffe...",
  "cwd": "/Users/.../hello-world",
  "timestamp": "2026-02-24T16:49:20.094Z",
  "tool_name": "edit",
  "tool_call_id": "toolu_01EFoT6mPt...",
  "status": "completed",
  "duration": 5,
  "modified_files": true,
  "tool_output": "Edit applied successfully.",
  "title": "Users/.../package.json",
  "file_path": "/Users/.../package.json",
  "filediff": {
    "file": "/Users/.../package.json",
    "before": "{\n  \"name\": \"hello-world\"...",
    "after": "{\n  \"name\": \"hello-world\"...",
    "additions": 4,
    "deletions": 2
  },
  "diagnostics": {
    "/Users/.../src/index.ts": []
  },
  "exit_code": null
}
```

---

## Rust-Side Processing Path

### 1. `opencode.rs` — Parse enriched payloads

`StopInput` and `AfterToolInput` deserialize the new fields. Because all new
fields use `#[serde(default)]`, old payloads from pre-update plugins still
parse correctly.

### 2. `opencode.rs` → `TurnEvent` — Thread new fields via `raw_json`

The `raw_json` field on `TurnEvent` already carries the full parsed JSON. The
orchestrator and `build_turn_provenance` read from it:

```rust
// In build_turn_provenance:
if let Some(raw) = &options.event.raw_json {
    if let Some(agent) = raw.get("agent").and_then(|v| v.as_str()) {
        provenance.agent_mode = Some(agent.to_string());
    }
    if let Some(reason) = raw.get("finish_reason").and_then(|v| v.as_str()) {
        provenance.finish_reason = Some(reason.to_string());
    }
    if let Some(steps) = raw.get("step_count").and_then(|v| v.as_u64()) {
        provenance.step_count = Some(steps as u32);
    }
    if let Some(slug) = raw.get("session_slug").and_then(|v| v.as_str()) {
        provenance.session_slug = Some(slug.to_string());
    }
    if let Some(sig) = raw.get("reasoning_signature").and_then(|v| v.as_str()) {
        provenance.reasoning_signature = Some(sig.to_string());
    }

    // Tokens (with reasoning)
    let input = raw.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = raw.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let reasoning = raw.get("reasoning_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_r = raw.get("cache_read_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_w = raw.get("cache_write_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    provenance.tokens = TokenUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output + reasoning,
        cache_read_tokens: cache_r,
        cache_write_tokens: cache_w,
        reasoning_tokens: reasoning,
    };

    // Cost
    if let Some(cost) = raw.get("cost_usd").and_then(|v| v.as_f64()) {
        provenance.cost = Cost::from_usd(cost);
    }
}
```

### 3. `accumulator.rs` → `ProvenanceGraph` — Richer nodes

The `ProvenanceAccumulator` builds `ProvenanceNode` entries from `after-tool`
events. With the enriched payloads, it can now populate `detail` with
structured JSON:

- **Commitment nodes** (write/edit): `detail` includes `filediff` and `diagnostics`
- **Execution nodes** (bash): `detail` includes `command`, `exit_code`, `output_summary`
- **Decision nodes** (from reasoning parts): `detail` includes `reasoning_text` and `anthropic_signature`
- **Goal nodes**: `detail` includes `prompt`, `agent_mode`, `todos`

### 4. Change serialization

All provenance data flows into `HashedChange`:
- `HashedChange.provenance: Vec<Provenance>` — the enriched per-change provenance
- `HashedChange.metadata: Vec<u8>` — the `SessionEnvelope` (turn number, timing, files)
- Provenance graph stored as a separate content-addressed artifact in `.atomic/provenance/`

---

## What This Enables

### `atomic log --provenance`

```
Change abc123 — "let's build a simple typescript hello world app"
  Author:    opencode (claude-sonnet-4-5 via anthropic)
  Mode:      build
  Session:   mighty-rocket (ses_36f52c606ffe...)
  Turn:      1 (14 steps, finished: stop)
  Tokens:    16 in / 1805 out / 0 reasoning / 167461 cache_r / 14437 cache_w
  Cost:      $0.2192
  Files:     4 files (+42 -3)
  Reasoning: 8 blocks, 1431 chars, 8.1s total
             ✓ Anthropic-signed (EucFCk...)

  Tool calls:
    bash      "npm init -y"                          exit:0  1.2s
    bash      "npm install typescript --save-dev"    exit:0  0.8s
    write     tsconfig.json                          +14     new file
    write     src/index.ts                           +5      new file
    bash      "npm run dev"                          exit:0  2.1s
    todowrite (4 items, 3 completed)
    ...
```

### `atomic log --reasoning`

```
Change abc123 — Turn 1

  [thinking 1] 2.8s
  The user wants to build a simple TypeScript hello world application.
  This is a straightforward task, so I shouldn't need to use the TodoWrite
  tool since it's a simple, single-step task.

  I'll need to:
  1. Check what's in the current directory
  2. Create a package.json file
  3. Create a tsconfig.json file
  ...

  [thinking 2] 0.9s
  The directory is relatively empty. There's a .atomic directory, an
  .atomicignore file, and a .src directory. Let me check what's in .src first.

  [thinking 3] 0.6s
  There's already an index.ts file. Let me check what's in it.
  ...
```

### Compliance API (via The Hive)

```json
{
  "change_hash": "blake3:abc123...",
  "provenance": {
    "vendor": "anthropic",
    "model": "claude-sonnet-4-5",
    "agent_mode": "build",
    "prompt": "let's build a simple typescript hello world app",
    "tokens": { "input": 16, "output": 1805, "reasoning": 0, "cache_read": 167461 },
    "cost_usd": 0.2192,
    "step_count": 14,
    "finish_reason": "stop",
    "reasoning_blocks": 8,
    "reasoning_signed": true,
    "session_slug": "mighty-rocket"
  },
  "file_changes": [
    {
      "path": "tsconfig.json",
      "operation": "create",
      "additions": 14,
      "deletions": 0,
      "diagnostics_clean": true
    },
    {
      "path": "src/index.ts",
      "operation": "create",
      "additions": 5,
      "deletions": 0,
      "diagnostics_clean": true
    }
  ],
  "tool_calls": [
    { "tool": "bash", "title": "npm init -y", "exit_code": 0 },
    { "tool": "write", "file": "tsconfig.json", "additions": 14 },
    { "tool": "bash", "title": "npm run dev", "exit_code": 0 }
  ]
}
```

---

## Multi-Agent Coverage Analysis

This section maps each provenance spec field to every supported agent, documenting
what is currently captured, what can be captured with code changes, and what is a
genuine gap. The six provenance dimensions from the Overview are used as the
organizing framework.

### Agent Hook Architecture Comparison

Before diving into field-level coverage, it's important to understand the
fundamental architectural differences between the two hook systems:

| Aspect | OpenCode | Claude Code |
|--------|----------|-------------|
| **Hook mechanism** | TypeScript plugin on event bus | Shell commands via `settings.json` |
| **Data richness** | Plugin accumulates metadata across events, sends enriched payloads | Lean per-event JSON; no cross-event accumulation |
| **Token/cost data** | Exposed via `step-finish` and `message.updated` events | Not exposed in any hook event |
| **Reasoning blocks** | Captured via `reasoning` part events with text + signature | Not exposed in hook events; may exist in transcript JSONL |
| **Structured diffs** | Plugin computes `filediff` (before/after/additions/deletions) | Not provided; `tool_input` has `old_string`/`new_string` for Edit |
| **Tool results** | `tool.execute.after` sends `output`, `metadata.filediff`, `metadata.diagnostics` | `PostToolUse` provides full `tool_response` object |
| **Transcript** | Not available (events are the data) | `transcript_path` to full conversation JSONL — richest raw data source |
| **Subagent tracking** | Not applicable (single-agent model) | `SubagentStart`/`SubagentStop` hooks with agent transcripts |
| **Task planning** | `todo.updated` event with structured todo list | `TaskCompleted` hook with task subject/description |
| **Session metadata** | `session.created`/`session.updated` with slug, title, summary | `SessionStart` with model/source; no slug or summary |

### Dimension 1: Who — Identity and Attribution

| Spec Field | OpenCode | Claude Code Current | Claude Code Possible | Gap |
|-----------|----------|-------------------|---------------------|-----|
| `vendor` | ✅ From `provider` field in stop payload | ✅ Hardcoded `Anthropic` (correct — Claude Code is always Anthropic) | ✅ | — |
| `model` | ✅ From `model` field in stop payload | ⚠️ From `SessionStart` only (e.g., `"claude-sonnet-4-5-20250929"`) | ✅ Thread model from SessionStart through session state | Model captured but not threaded to stop event |
| `model_version` | ⚠️ Never populated | ⚠️ Available in `SessionStart.model` (includes version suffix) | ✅ Parse version from model string | Parse `"-20250929"` suffix |
| `tool` (orchestrator) | ✅ `Cli("opencode")` | ✅ `Cli("claude-code")` | ✅ | — |
| `session_id` | ✅ From all events | ✅ From all events | ✅ | — |
| `prompt` (user intent) | ✅ From `chat.message` hook | ✅ From `UserPromptSubmit` hook | ✅ | — |
| `agent_mode` | ✅ `"build"`, `"code"`, `"ask"` from `chat.message.input.agent` | ❌ Not a Claude Code concept | 🟡 Infer from `permission_mode`: `"dontAsk"` ≈ build, `"default"` ≈ code, `"plan"` ≈ ask | Semantic approximation only |

### Dimension 2: What — File Changes and Structured Diffs

| Spec Field | OpenCode | Claude Code Current | Claude Code Possible | Gap |
|-----------|----------|-------------------|---------------------|-----|
| `file_path` | ✅ From `after-tool.file_path` | ⚠️ Available in `tool_input.file_path` but not extracted | ✅ Extract from `PostToolUse.tool_input.file_path` (Write/Edit) | Code change only |
| `operation` (create/edit/delete) | ✅ Inferred from tool name | ⚠️ Inferred from tool name only (`write`→create, `edit`→edit) | ✅ Also check `tool_response.success` and file existence | Code change only |
| `additions` | ✅ From `filediff.additions` | ❌ Not captured | 🟡 Compute from Edit `new_string` line count or Write `content` line count | Computed, not exact diff |
| `deletions` | ✅ From `filediff.deletions` | ❌ Not captured | 🟡 Compute from Edit `old_string` line count | Computed, not exact diff |
| `filediff` (before/after content) | ✅ Full structured diff from plugin | ❌ Not provided by hooks | 🟡 Self-compute: read file at `PreToolUse`, compare at `PostToolUse` | Requires file I/O in hook scripts |
| `diagnostics` (LSP) | ✅ From `after-tool.metadata.diagnostics` | ❌ Not provided by hooks | 🟡 Self-run: execute LSP diagnostics post-edit via async `PostToolUse` hook | Requires external tooling |
| `before_hash` / `after_hash` | ❌ Not yet implemented | ❌ Not captured | 🟡 Hash file at `PreToolUse` and `PostToolUse` | Same effort both agents |

### Dimension 3: Why — Intent, Reasoning, and Task Plan

| Spec Field | OpenCode | Claude Code Current | Claude Code Possible | Gap |
|-----------|----------|-------------------|---------------------|-----|
| `prompt` (full text) | ✅ From `chat.message.output.parts[].text` | ✅ From `UserPromptSubmit.prompt` | ✅ | — |
| `reasoning_text` | ✅ From `reasoning` part events (concatenated blocks) | ❌ Not exposed in hooks | 🟡 Parse from `transcript_path` JSONL — assistant messages may contain thinking blocks | Transcript mining required |
| `reasoning_duration_ms` | ✅ From `reasoning.time.end - time.start` | ❌ Not exposed | 🟡 May be derivable from transcript timestamps | Transcript mining required |
| `reasoning_signature` | ✅ From `reasoning.metadata.anthropic.signature` | ❌ Not exposed in hooks | ❌ Not available in transcript either | **Genuine gap** |
| `task_plan` / `todos` | ✅ From `todo.updated` event | ❌ `TaskCompleted` hook exists but not installed | 🟡 Install `TaskCompleted` hook — provides `task_subject`, `task_description` per completed task | New hook installation |
| `last_assistant_message` | ❌ Not available (no equivalent) | ⚠️ Available in `Stop.last_assistant_message` but not captured | ✅ Extract from Stop event — agent's final summary of what it did | **Claude Code exclusive** |
| `session_slug` | ✅ From `session.created.info.slug` | ❌ Not a Claude Code concept | 🟡 Generate deterministic slug from `session_id` | Synthetic, not agent-native |

### Dimension 4: How — Tool Calls, Timeline, and Subagents

| Spec Field | OpenCode | Claude Code Current | Claude Code Possible | Gap |
|-----------|----------|-------------------|---------------------|-----|
| `tool_name` | ✅ From `tool.execute.after.input.tool` | ✅ From `PostToolUse.tool_name` | ✅ | — |
| `tool_call_id` | ✅ From `tool.execute.after.input.callID` | ✅ From `PostToolUse.tool_use_id` | ✅ | — |
| `tool_input` (args) | ✅ From `tool.execute.after.input.args` | ✅ From `PostToolUse.tool_input` (full structured args) | ✅ | — |
| `tool_output` | ✅ From `tool.execute.after.output.output` | ⚠️ Available in `PostToolUse.tool_response` but not fully extracted | ✅ Parse `tool_response` per tool type | Code change only |
| `tool_status` | ✅ From `after-tool.status` ("completed"/"error") | ⚠️ Implicit: `PostToolUse` = success, `PostToolUseFailure` = error | ✅ Install `PostToolUseFailure` hook for error tracking | New hook installation |
| `tool_duration` | ✅ From `after-tool.duration` | ❌ Not provided in hook JSON | ⚠️ Could time between `PreToolUse` and `PostToolUse` in Rust | Requires state tracking |
| `exit_code` (Bash) | ✅ From `after-tool.metadata.exit` | ⚠️ Available in `tool_response` but not extracted | ✅ Parse from Bash `tool_response` | Code change only |
| `title` (human description) | ✅ From `after-tool.output.title` | ❌ Not provided | 🟡 Use Bash `tool_input.description` or Write/Edit `tool_input.file_path` | Partial — Bash has descriptions |
| `error` (failure detail) | ❌ Only via output heuristics | ❌ `PostToolUseFailure` not installed | ✅ Install `PostToolUseFailure` → `error` field + `is_interrupt` | New hook installation |
| **Subagent tracking** | ❌ Not applicable | ❌ `SubagentStart`/`SubagentStop` not installed | ✅ Install both → `agent_type`, `agent_transcript_path`, `last_assistant_message` | **Claude Code exclusive** — new provenance node type |
| `step_count` | ✅ Derived from `step-finish` event count | ⚠️ Not captured | 🟡 Count `PostToolUse` events per turn in session state | Derived, not exact (misses non-tool LLM steps) |

### Dimension 5: How Much — Tokens, Cost, Duration

| Spec Field | OpenCode | Claude Code Current | Claude Code Possible | Gap |
|-----------|----------|-------------------|---------------------|-----|
| `input_tokens` | ✅ From `step-finish.tokens.input` | ❌ Not exposed in any hook | ❌ Not available | **Genuine gap** |
| `output_tokens` | ✅ From `step-finish.tokens.output` | ❌ Not exposed | ❌ Not available | **Genuine gap** |
| `reasoning_tokens` | ✅ From `step-finish.tokens.reasoning` | ❌ Not exposed | ❌ Not available | **Genuine gap** |
| `cache_read_tokens` | ✅ From `step-finish.tokens.cache.read` | ❌ Not exposed | ❌ Not available | **Genuine gap** |
| `cache_write_tokens` | ✅ From `step-finish.tokens.cache.write` | ❌ Not exposed | ❌ Not available | **Genuine gap** |
| `cost_usd` | ✅ From `step-finish.cost` | ❌ Not exposed | 🟡 Estimate from model pricing tables if token counts become available | Blocked by token gap |
| `turn_duration_ms` | ✅ From plugin-measured wall clock | ⚠️ Measured Rust-side (CLI invocation timing, not wall clock) | 🟡 Improve: time from `UserPromptSubmit` to `Stop` in session state | Approximation |
| `finish_reason` | ✅ From `step-finish.reason` ("stop", "tool-calls") | ⚠️ Not directly exposed | 🟡 Infer from `Stop.stop_hook_active`: `true` → "tool-calls", `false` → "stop" | Semantic approximation |

> **Note on token/cost gap**: Claude Code does not expose token usage or cost
> in any hook event. This is the single largest provenance gap compared to
> OpenCode. Possible future mitigations:
> - Anthropic adds token/cost fields to the `Stop` hook input
> - Parse the Anthropic API admin console or billing API
> - Estimate tokens from transcript message byte sizes (very rough)
> - Use a proxy that logs API responses

### Dimension 6: How Trustworthy — Signatures, Diagnostics, Verification

| Spec Field | OpenCode | Claude Code Current | Claude Code Possible | Gap |
|-----------|----------|-------------------|---------------------|-----|
| `reasoning_signature` | ✅ From `reasoning.metadata.anthropic.signature` | ❌ Not exposed in hooks | ❌ Not available in transcript | **Genuine gap** |
| `diagnostics` (LSP) | ✅ From `after-tool.metadata.diagnostics` | ❌ Not provided | 🟡 Self-run: async `PostToolUse` hook runs diagnostics on edited files | Requires tooling setup |
| `diagnostics_clean` | ✅ Derived from diagnostics emptiness | ❌ Not available | 🟡 Derived from self-run diagnostics | Depends on above |
| `passed` (test/build result) | ✅ Derived from Bash output heuristics | ⚠️ Available in `tool_response` but not extracted for verification | ✅ Parse Bash `tool_response` for exit codes | Code change only |
| `permission_mode` | ❌ Not available | ⚠️ Available in every hook but not captured | ✅ Store in session state — indicates trust level | **Claude Code exclusive** |

### Coverage Summary

| Provenance Dimension | OpenCode | Claude Code (Current) | Claude Code (Achievable) | Genuine Gaps |
|---------------------|----------|----------------------|------------------------|-------------|
| **1. Who** (identity) | ✅ Full | ⚠️ Partial — model from SessionStart only | ✅ Full (thread model, infer mode) | `agent_mode` is approximated |
| **2. What** (file changes) | ✅ Full — plugin provides structured diffs | ⚠️ Minimal — tool name only | 🟡 Good — compute diffs from `tool_input` | `filediff` is computed not native |
| **3. Why** (intent/reasoning) | ✅ Full — reasoning blocks + todos | ⚠️ Prompt only | 🟡 Good — transcript mining + TaskCompleted | `reasoning_signature` unavailable |
| **4. How** (tool calls) | ✅ Full | ⚠️ Partial — PostToolUse only | ✅ Full — add failure/subagent hooks | Subagent tracking is **Claude Code exclusive** |
| **5. How much** (cost) | ✅ Full — tokens, cost, duration | ❌ None | ❌ Minimal — duration only | **Tokens and cost not exposed** |
| **6. How trustworthy** | ✅ Full — signatures + diagnostics | ❌ None | 🟡 Partial — self-run diagnostics | `reasoning_signature` unavailable |

### Claude Code Exclusive Capabilities

Claude Code provides several data sources that OpenCode does not:

| Capability | Hook/Source | Provenance Value |
|-----------|------------|-----------------|
| **Full conversation transcript** | `transcript_path` on every event | Complete session reconstruction; reasoning block extraction via transcript mining |
| **Subagent lifecycle** | `SubagentStart` / `SubagentStop` hooks | Delegation provenance — which subtasks were spawned, what agents ran them, what they concluded |
| **Subagent transcripts** | `SubagentStop.agent_transcript_path` | Full conversation history for each subagent |
| **Agent final summary** | `Stop.last_assistant_message` | The model's own summary of what it accomplished |
| **Permission mode** | `permission_mode` on every event | Trust context — was the agent running autonomously or with human approval gates? |
| **Session start source** | `SessionStart.source` | Whether session is fresh (`startup`), resumed (`resume`), or compacted (`compact`) |
| **Task completion events** | `TaskCompleted` hook | Structured task-level progress with subject and description |
| **Tool failure detail** | `PostToolUseFailure` hook | Error messages and interrupt flags for failed tool calls |

### Claude Code Hooks — Current vs Proposed Installation

The current `ClaudeCodeHook::install()` registers 7 hooks. The proposed
expansion adds 4 new hooks to capture richer provenance data:

| Hook Event | Matcher | Current | Proposed | Provenance Purpose |
|-----------|---------|---------|----------|-------------------|
| `SessionStart` | — | ✅ Installed | ✅ | Session init, model capture |
| `SessionEnd` | — | ✅ Installed | ✅ | Session teardown, attestation |
| `Stop` | — | ✅ Installed | ✅ **Enrich**: capture `last_assistant_message`, `stop_hook_active` | Turn boundary, agent summary |
| `UserPromptSubmit` | — | ✅ Installed | ✅ **Enrich**: capture `permission_mode` | User intent |
| `PreToolUse[Task]` | `Task` | ✅ Installed | ✅ | Sub-agent pre-hook |
| `PostToolUse[Task]` | `Task` | ✅ Installed | ✅ **Enrich**: extract `tool_response` | Sub-agent result |
| `PostToolUse[TodoWrite]` | `TodoWrite` | ✅ Installed | ✅ | Task plan capture |
| `PostToolUseFailure` | `*` | ❌ | 🆕 **Add** | Error provenance for all tools |
| `SubagentStart` | `*` | ❌ | 🆕 **Add** | Delegation tracking |
| `SubagentStop` | `*` | ❌ | 🆕 **Add** | Subagent result + transcript |
| `TaskCompleted` | — | ❌ | 🆕 **Add** | Task completion tracking |

### Claude Code Enrichment — Implementation Tiers

#### Tier 1: Extract more from existing hook data (no new hooks)

| Change | Effort | Fields Gained |
|--------|--------|--------------|
| Parse `tool_response` in `handle_tool_use` for Claude Code agent | Small | `exit_code`, `success`, `file_path` from tool results |
| Compute additions/deletions from Edit `old_string`/`new_string` | Small | `additions`, `deletions` on Commitment nodes |
| Capture `last_assistant_message` from Stop event | Small | Turn summaries, `finish_reason` inference |
| Thread `model` from SessionStart through session state to Stop | Small | Accurate `model` on every change |
| Store `permission_mode` in session state | Small | Trust context metadata |
| Derive `step_count` from PostToolUse event count per turn | Small | `step_count` field |

#### Tier 2: Install new hooks (new hook types, small Rust changes)

| Change | Effort | Fields Gained |
|--------|--------|--------------|
| Install `PostToolUseFailure` hook | Medium | Error provenance: `error`, `is_interrupt`, failed tool detail |
| Install `SubagentStart` / `SubagentStop` hooks | Medium | Delegation provenance: `agent_type`, `agent_transcript_path`, subagent results |
| Install `TaskCompleted` hook | Medium | Task completion: `task_subject`, `task_description` |

#### Tier 3: Transcript mining (deeper analysis at turn-end)

| Change | Effort | Fields Gained |
|--------|--------|--------------|
| Parse transcript JSONL tail at `handle_turn_end` | Large | Reasoning/thinking blocks, complete tool timeline, multi-turn context |
| Extract thinking blocks from assistant messages | Large | `reasoning_text`, `reasoning_duration_ms` (if timestamps present) |
| Reconstruct full tool call timeline from transcript | Medium | Supplement/verify hook-captured data; recover from missed events |

#### Tier 4: Self-computed enrichment (external tooling)

| Change | Effort | Fields Gained |
|--------|--------|--------------|
| Compute `filediff` by reading file at PreToolUse and comparing at PostToolUse | Large | `filediff.before`, `filediff.after`, `additions`, `deletions` (exact) |
| Run LSP diagnostics post-edit via async PostToolUse hook | Large | `diagnostics`, `diagnostics_clean` |
| Hash files at PreToolUse/PostToolUse for content addressing | Medium | `before_hash`, `after_hash` |

---

## Implementation Order — OpenCode (Phases 1–10)

| Phase | Scope | What Changes |
|-------|-------|-------------|
| **1** | `TokenUsage` | Add `reasoning_tokens: u64` field |
| **2** | `Provenance` | Add `agent_mode`, `finish_reason`, `step_count`, `session_slug`, `reasoning_signature` |
| **3** | `StopInput` | Accept new fields from plugin (`agent`, `reasoning_tokens`, `finish_reason`, `step_count`, `session_slug`, `reasoning_text`, `reasoning_signature`, `todos`) |
| **4** | `AfterToolInput` | Accept new fields (`title`, `file_path`, `filediff`, `diagnostics`, `exit_code`) |
| **5** | `UserPromptInput` | Accept `agent` field |
| **6** | `build_turn_provenance` | Read new fields from `raw_json`, populate `Provenance` |
| **7** | Plugin `stop` payload | Send accumulated reasoning text, signatures, todos, step count, finish reason, session slug |
| **8** | Plugin `after-tool` payload | Already sending filediff, diagnostics, exit_code, title (done in current PR) |
| **9** | `ProvenanceAccumulator` | Build richer node details from enriched after-tool events |
| **10** | `atomic log` | Display new provenance fields |

Phases 1–6 are Rust changes in `atomic-core` and `atomic-agent`.
Phases 7–8 are TypeScript changes in `opencode-atomic-hooks`.
Phase 7 is partially done (plugin already sends `agent`, `reasoning_tokens`).
Phase 8 is done (plugin already enriches `after-tool` with filediff, diagnostics, etc.).
Phase 9 requires changes to the accumulator's node-building logic.
Phase 10 is CLI display.

## Implementation Order — Claude Code (Phases 11–18)

| Phase | Scope | What Changes |
|-------|-------|-------------|
| **11** | `handle_tool_use` | Agent-aware `tool_response` parsing: extract `exit_code`, `file_path`, `success` from Claude Code's `PostToolUse.tool_response` instead of OpenCode-style top-level keys |
| **12** | `build_tool_detail` | Compute `additions`/`deletions` from Edit `old_string`/`new_string`; use Write `content` line count for creates |
| **13** | `handle_turn_end` | Capture `last_assistant_message` from Stop `raw_json`; infer `finish_reason` from `stop_hook_active` |
| **14** | `ClaudeCodeHook::install` | Add `PostToolUseFailure` (matcher `*`), `SubagentStart` (matcher `*`), `SubagentStop` (matcher `*`), `TaskCompleted` hooks |
| **15** | `ClaudeCodeHook::parse_event` | Parse new hook types: `PostToolUseFailure` → Error nodes, `SubagentStart`/`SubagentStop` → Delegation nodes, `TaskCompleted` → Goal enrichment |
| **16** | `HookType` / `TurnEvent` | Extend with new event types or map Claude Code-specific events to existing types |
| **17** | Transcript mining | At `handle_turn_end`, parse `transcript_path` JSONL tail for reasoning/thinking blocks → Decision nodes |
| **18** | Self-compute diffs | Optional: read file at `PreToolUse`, hash/store; compare at `PostToolUse` for `filediff` |

Phases 11–13 are Rust changes in `atomic-agent` (orchestrator + record).
Phase 14 is a settings.json change (hook installation).
Phases 15–16 are Rust changes in `atomic-agent` (hook parsing + event model).
Phase 17–18 are larger Rust changes requiring file I/O and transcript parsing.

---

## Backward Compatibility

All changes are additive:

- **Rust structs**: Every new field is `Option<T>` with `#[serde(default)]`.
  Existing serialized changes deserialize without error. New fields default to
  `None` / `0`.

- **Plugin → CLI**: The Rust `StopInput` / `AfterToolInput` use `#[serde(default)]`
  on every field. An old plugin that doesn't send `reasoning_tokens` will
  deserialize as `None`. A new plugin sending to an old CLI will have the extra
  fields silently ignored by serde.

- **Change format**: The `Provenance` section in the V3 change format is
  postcard-serialized. Postcard handles `Option::None` and `default` fields
  gracefully. Old changes read with new code get `None` for new fields.
  New changes read with old code skip unknown fields (postcard is
  forward-compatible with `#[serde(default)]`).

- **ProvenanceGraph**: Schema version is already tracked (`version: u8`).
  Bump to version 2 when node detail schemas change. Version 1 graphs
  still deserialize; the new detail fields are simply absent.