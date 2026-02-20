# Provenance Graph — Architecture Plan

> **Goal**: Capture the causal decision chain of agentic coding sessions as a
> queryable, content-addressed DAG — not a timeline, not a commit log, not a
> chat transcript. When something breaks, you traverse the graph from the
> failing patch back through the decision chain to the human intent that
> spawned it. Ten seconds instead of an hour of git blame.

---

## Table of Contents

1. [Motivation](#1-motivation)
2. [Architecture Overview](#2-architecture-overview)
3. [Phase 1 — Graph Accumulator (Rust, atomic-agent)](#3-phase-1--graph-accumulator-rust-atomic-agent)
4. [Phase 2 — Provenance Storage Schema (Rust, atomic-core)](#4-phase-2--provenance-storage-schema-rust-atomic-core)
5. [Phase 3 — Classification Layer](#5-phase-3--classification-layer)
6. [Phase 4 — Compaction Hook (Plugin)](#6-phase-4--compaction-hook-plugin)
7. [Phase 5 — WebUI](#7-phase-5--webui)
8. [Cross-Cutting Concerns](#8-cross-cutting-concerns)
9. [Open Questions](#9-open-questions)
10. [Appendix — Existing Infrastructure Map](#10-appendix--existing-infrastructure-map)

---

## 1. Motivation

### What we have today

The existing Atomic plugin for OpenCode (`opencode/.opencode/plugins/atomic/`)
is a **thin pipe**. It translates OpenCode lifecycle events into
`atomic agent hooks opencode <verb>` CLI calls — sequentially, statelessly,
fire-and-forget. The Rust-side `TurnOrchestrator` records each turn as an
Atomic change with provenance metadata (model, tokens, cost) and an optional
transcript in the unhashed section. Attestations summarize sessions after the
fact.

This gives us **what** changed and **who** changed it (human vs. agent). It
does not give us **why** — the chain of exploration, reasoning, dead ends,
and commitments that led to the final patch.

### What we want

A **provenance graph** that models agentic work as a directed acyclic graph of
typed nodes and causal edges:

```
Human goal: "Fix the auth bug"
    │
    ├─→ Exploration: read auth.rs, read jwt.rs, read middleware.rs
    │       │
    │       └─→ Decision: "Token expiry uses wrong timezone"
    │               │
    │               ├─→ Commitment: edit auth.rs (patch A)
    │               │
    │               └─→ Verification: cargo test --lib
    │                       │
    │                       └─→ Patch Proposal: change hash ABCD...
    │
    └─→ Human Gate: "Should I also fix the refresh endpoint?"
```

This graph is:
- **Built in real-time** during the session (not reconstructed after)
- **Agent-agnostic** (works for Claude Code, Gemini CLI, Codex, OpenCode — any
  agent that flows through `TurnOrchestrator`)
- **Persistent across hook invocations** (durable in `.atomic/sessions/`)
- **Content-addressed** (stored in Atomic's graph, pushed to remotes)
- **Queryable** (given a patch, walk backward to the goal that motivated it)
- **Mergeable across sessions** (session subgraphs compose into a project-level
  intent graph)

### Why not just build another GitHub UI

GitHub's model — commits, PRs, blame, comment threads — was designed for
humans writing code manually. The primitives assume:
- One commit = one logical change (rarely true with agents)
- PR description explains intent (agents don't write meaningful PR descriptions)
- Code review catches reasoning errors (you can't review reasoning you can't see)

Agentic work needs different primitives:
- **Goals** (what the human asked for)
- **Decisions** (what strategy the agent chose, and what it rejected)
- **Commitments** (which decisions produced file changes)
- **Verifications** (how the agent validated its work)
- **Human gates** (where the agent was uncertain and asked for permission)

These aren't decorations on top of diffs. They're the primary unit of
understanding.

---

## 2. Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        Provenance Graph Architecture                        │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  Agent (Claude Code / Gemini CLI / Codex / OpenCode)                        │
│       │                                                                     │
│       │  Hook callbacks (JSON on stdin)                                     │
│       ▼                                                                     │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │  atomic agent hooks <agent> <verb>                                    │  │
│  │                                                                       │  │
│  │  AgentHook::parse_event() → TurnEvent                                 │  │
│  │       │                                                               │  │
│  │       ▼                                                               │  │
│  │  TurnOrchestrator::dispatch(event)                                    │  │
│  │       │                                                               │  │
│  │       ├──▶ Existing: phase transitions, record_turn(), attestation    │  │
│  │       │                                                               │  │
│  │       └──▶ NEW: ProvenanceAccumulator::append(event)                  │  │
│  │                 │                                                     │  │
│  │                 ├── Classify tool call (rule-based)                    │  │
│  │                 ├── Create typed graph node                            │  │
│  │                 ├── Infer causal edges from session context            │  │
│  │                 └── Persist graph to .atomic/sessions/{id}/graph.json │  │
│  │                                                                       │  │
│  │  On TurnEnd: save ProvenanceGraph artifact alongside Change           │  │
│  │  On SessionEnd: finalize graph, create content-addressed artifact     │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ │
│                                                                             │
│  OpenCode Plugin (thin — unchanged from current design)                     │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │  .opencode/plugins/atomic/                                            │  │
│  │                                                                       │  │
│  │  Existing handlers: event → CLI pipe, chat → CLI pipe, tool → CLI pipe│  │
│  │                                                                       │  │
│  │  NEW (tiny):                                                          │  │
│  │    experimental.session.compacting →                                   │  │
│  │      read .atomic/sessions/{id}/graph.json                            │  │
│  │      serialize summary into compaction context                        │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ │
│                                                                             │
│  the-hive WebUI (React)                                                     │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │  /tenants/:t/portfolios/:p/projects/:proj/provenance                  │  │
│  │                                                                       │  │
│  │  ┌─────────────┐  ┌──────────────────┐  ┌─────────────────────────┐  │  │
│  │  │ Intent      │  │ Node             │  │ Session                 │  │  │
│  │  │ Graph       │  │ Inspector        │  │ Timeline                │  │  │
│  │  │ (DAG view)  │  │ (detail panel)   │  │ (secondary/debug)      │  │  │
│  │  └─────────────┘  └──────────────────┘  └─────────────────────────┘  │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Why the graph accumulator lives in Rust (atomic-agent), not the plugin

1. **The plugin is a thin pipe by design.** Every handler does one thing: build
   a JSON payload, call `invokeHook($, verb, payload, directory)`, log the
   result. The plugin's `SessionStore` is explicitly ephemeral. Making the
   plugin thick fights the architecture.

2. **TurnOrchestrator already dispatches every event.** `SessionStart`,
   `TurnStart`, `TurnEnd`, `PreToolUse`, `PostToolUse`, `SessionEnd` — the
   orchestrator sees them all and maintains durable session state in
   `.atomic/sessions/`. The graph accumulator is another piece of state the
   orchestrator maintains on each dispatch, persisted alongside `AgentSession`.

3. **It works for every agent.** Claude Code, Gemini CLI, Codex, OpenCode all
   flow through `TurnOrchestrator::dispatch()`. Build the graph in Rust and
   every agent gets provenance graphs for free. Build it in the OpenCode plugin
   and it's OpenCode-only.

4. **The data is already there.** `TurnEvent` carries tool names, tool use IDs,
   raw JSON with tool args/output, prompts, and timestamps. `AfterToolInput`
   (from the OpenCode hook parser) already has `tool_output`, `status`,
   `duration`, and `modified_files`. All the classification signals are flowing
   through the Rust side today — `handle_tool_use` just logs them and returns.

5. **Persistence is free.** `AgentSession` is already JSON-serialized to
   `.atomic/sessions/{id}.json` on every hook callback. The graph can live in a
   sibling file (`.atomic/sessions/{id}/graph.json`) or as a new field on
   `AgentSession` itself. No new persistence infrastructure needed.

6. **The compaction hook is the only plugin-side piece**, and it's trivially
   thin — read the graph file from disk, format a text summary, push it into
   the compaction context. A few lines of code.

### Data flow summary

1. **Agent fires a hook** → Plugin pipes JSON to `atomic agent hooks <agent> <verb>`.
2. **Rust parses the event** → `AgentHook::parse_event()` produces a `TurnEvent`.
3. **Orchestrator dispatches** → Existing phase transitions + turn recording.
4. **NEW: Orchestrator appends to graph** → `ProvenanceAccumulator::append(event)`
   classifies the tool call, creates a typed node, infers causal edges, persists.
5. **On TurnEnd** → Graph snapshot saved as part of turn metadata.
6. **On SessionEnd** → Final graph saved as a content-addressed `ProvenanceGraph`
   artifact (Phase 2).
7. **On push** → Provenance graph travels with the changes it references.
8. **WebUI** → Queries the API for provenance graphs and renders them as DAGs.

---

## 3. Phase 1 — Graph Accumulator (Rust, atomic-agent)

> **Duration**: ~2 weeks
> **Scope**: Rust, `atomic-agent` crate
> **Dependency**: None — builds on existing `TurnOrchestrator` dispatch pipeline

### 3.1 New modules

```
atomic-agent/src/
├── ... (existing files unchanged)
├── provenance/
│   ├── mod.rs             ← Module root, re-exports
│   ├── types.rs           ← Graph node/edge type definitions
│   ├── accumulator.rs     ← In-memory DAG builder + disk persistence
│   ├── classify.rs        ← Rule-based tool call classification
│   └── serialize.rs       ← Graph → JSON, Graph → compaction text
```

### 3.2 Node types

```rust
// atomic-agent/src/provenance/types.rs

/// The kind of a provenance graph node.
///
/// Each node represents a distinct type of agent activity. The graph
/// captures the causal relationships between these activities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Human prompt — what was asked for.
    Goal,
    /// Read/search/grep — understanding the codebase.
    Exploration,
    /// Consolidated: agent chose strategy X over Y.
    Decision,
    /// Write/edit/patch — file changes on disk.
    Commitment,
    /// Test/lint/typecheck — validating work.
    Verification,
    /// Bash (non-test) — side effects (install, build, etc.).
    Execution,
    /// Permission asked — agent uncertainty surfaced to human.
    HumanGate,
    /// Session diff / recorded change — the output artifact.
    PatchProposal,
    /// Tool failure or session error.
    Error,
}

/// A node in the provenance graph.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphNode {
    /// Unique within this session graph: `{session_id}-{counter}`.
    pub id: String,

    /// What kind of activity this represents.
    pub kind: NodeKind,

    /// When this activity occurred (Unix epoch ms).
    pub timestamp: i64,

    /// One-line human-readable summary.
    /// Examples:
    ///   "Fix the auth bug in login.rs"       (goal)
    ///   "Read src/auth/login.rs"              (exploration)
    ///   "Edit src/auth/login.rs"              (commitment)
    ///   "cargo test --lib"                    (verification)
    pub summary: String,

    /// Kind-specific structured data.
    ///
    /// For exploration: { "files_read": ["src/auth.rs", "src/jwt.rs"] }
    /// For commitment: { "files_modified": ["src/auth.rs"], "tool": "Edit" }
    /// For verification: { "command": "cargo test", "passed": true }
    /// For human_gate: { "reason": "...", "resolved": false }
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,

    /// Link to the Atomic change this node produced (commitment/patch nodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_hash: Option<String>,

    /// Tool name that produced this node (for tool-derived nodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,

    /// Tool call ID for correlating pre/post pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Whether this node was consolidated by the classifier (Phase 3).
    #[serde(default)]
    pub classified: bool,

    /// Classifier confidence (0.0–1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,

    /// IDs of raw tool call nodes this decision consolidates (Phase 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consolidated_from: Vec<String>,
}

/// A causal edge between two nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

/// The kind of causal relationship between nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A led to B (forward-causal).
    LedTo,
    /// Decision was informed by exploration.
    ExploredVia,
    /// Decision produced a file commitment.
    CommittedVia,
    /// Commitment was validated by a verification.
    VerifiedBy,
    /// Human gate blocked progress.
    BlockedBy,
    /// Work continued after human gate resolved.
    ResumedAfter,
    /// Tool call produced an error.
    FailedWith,
}

/// Aggregate statistics for a provenance graph.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GraphStats {
    pub goal_count: u32,
    pub exploration_count: u32,
    pub decision_count: u32,
    pub commitment_count: u32,
    pub verification_count: u32,
    pub human_gate_count: u32,
    pub error_count: u32,
    pub execution_count: u32,
    pub patch_proposal_count: u32,
    pub edge_count: u32,
}
```

### 3.3 Rule-based classifier

```rust
// atomic-agent/src/provenance/classify.rs

/// Classify a tool call into a NodeKind based on tool name and arguments.
///
/// This is the deterministic, rule-based classifier (Tier 1). It handles
/// the 80% case. The LLM-powered classifier (Phase 3) consolidates
/// sequences of these into named Decision nodes.
pub fn classify_tool_call(
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    tool_output: Option<&str>,
    status: Option<&str>,
) -> NodeKind {
    // Error status always produces an error node
    if status == Some("error") {
        return NodeKind::Error;
    }

    match tool_name.to_lowercase().as_str() {
        // Read-family tools → exploration
        "read" | "read_file" | "readfile"
        | "grep" | "glob" | "find_path"
        | "list_directory" | "listdir"
        | "search" | "thinking" => NodeKind::Exploration,

        // Write-family tools → commitment
        "edit" | "edit_file" | "editfile"
        | "write" | "write_file" | "writefile"
        | "multiedit" | "multi_edit"
        | "patch" | "create" | "insert"
        | "todocreate" | "todowrite" => NodeKind::Commitment,

        // Shell commands: inspect the command to sub-classify
        "bash" | "terminal" | "shell" | "command" => {
            classify_shell_command(tool_input, tool_output)
        }

        // Task/sub-agent → treat as execution
        "task" | "subagent" => NodeKind::Execution,

        // Unknown → conservative default
        _ => NodeKind::Execution,
    }
}

/// Sub-classify a shell command into verification, execution, or commitment.
fn classify_shell_command(
    tool_input: Option<&serde_json::Value>,
    _tool_output: Option<&str>,
) -> NodeKind {
    let cmd = tool_input
        .and_then(|v| v.get("command").or_else(|| v.get("cmd")))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if is_test_command(cmd) {
        return NodeKind::Verification;
    }
    if is_lint_command(cmd) {
        return NodeKind::Verification;
    }
    if is_build_command(cmd) {
        return NodeKind::Verification;
    }

    NodeKind::Execution
}

fn is_test_command(cmd: &str) -> bool {
    // Matches: cargo test, npm test, bun test, pytest, jest, vitest, go test, etc.
    let patterns = [
        "test", "spec", "jest", "vitest", "pytest", "phpunit",
        "cargo test", "cargo nextest", "go test",
        "bun test", "npm test", "npm run test",
        "yarn test", "pnpm test",
    ];
    let lower = cmd.to_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}

fn is_lint_command(cmd: &str) -> bool {
    let patterns = [
        "lint", "eslint", "clippy", "tsc --noEmit", "typecheck",
        "pylint", "flake8", "ruff", "mypy", "prettier --check",
        "cargo clippy", "cargo fmt -- --check",
    ];
    let lower = cmd.to_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}

fn is_build_command(cmd: &str) -> bool {
    let patterns = [
        "cargo build", "cargo check",
        "npm run build", "bun build",
        "tsc", "go build", "make",
    ];
    let lower = cmd.to_lowercase();
    // Avoid false positives: "tsc" alone is a build, but "tsc --noEmit" is lint
    patterns.iter().any(|p| lower.contains(p))
}
```

### 3.4 The accumulator

The `ProvenanceAccumulator` maintains the in-memory graph for a session and
persists it to disk after every event. It lives alongside `AgentSession` — the
orchestrator loads it, appends to it, and saves it on every dispatch.

```rust
// atomic-agent/src/provenance/accumulator.rs

/// In-memory DAG builder for a single session's provenance graph.
///
/// Maintains the graph state and infers causal edges based on the
/// sequence of events and their classifications.
///
/// # Persistence
///
/// The accumulator serializes to `.atomic/sessions/{session_id}/graph.json`
/// after every append. This ensures the graph survives process exits
/// (each hook invocation is a separate process).
///
/// # Edge Inference
///
/// Edges are inferred from the event sequence, not explicitly provided:
///
/// | Event sequence             | Edge inferred                          |
/// |----------------------------|----------------------------------------|
/// | Goal → any tool call       | Goal --led_to-→ tool node              |
/// | Exploration → Commitment   | Exploration --explored_via-→ Commitment|
/// | Commitment → Verification  | Commitment --verified_by-→ Verification|
/// | Any → Error                | Source --failed_with-→ Error            |
/// | HumanGate → next activity  | HumanGate --resumed_after-→ next       |
pub struct ProvenanceAccumulator {
    /// Session this graph belongs to.
    session_id: String,

    /// All nodes in the graph, keyed by ID.
    nodes: IndexMap<String, GraphNode>,

    /// All edges in the graph.
    edges: Vec<GraphEdge>,

    /// Monotonic counter for generating unique node IDs.
    counter: u64,

    // ---- Edge inference state ----

    /// Most recent goal node ID.
    current_goal: Option<String>,

    /// Exploration nodes since the last decision/commitment.
    pending_explorations: Vec<String>,

    /// Most recent commitment node ID (for verification edges).
    last_commitment: Option<String>,

    /// Most recent node ID (for sequential edge fallback).
    last_node: Option<String>,

    /// Whether a human gate is currently blocking.
    pending_human_gate: Option<String>,
}

impl ProvenanceAccumulator {
    /// Create a new empty accumulator for a session.
    pub fn new(session_id: impl Into<String>) -> Self { ... }

    /// Load from disk, or create empty if no persisted graph exists.
    pub fn load_or_create(session_dir: &Path, session_id: &str) -> AgentResult<Self> { ... }

    /// Append a goal node (human prompt).
    pub fn append_goal(&mut self, prompt: &str, timestamp: i64) -> String { ... }

    /// Append a tool call node, classified by the rule-based classifier.
    ///
    /// This is the primary entry point called from `handle_tool_use`.
    pub fn append_tool_call(
        &mut self,
        tool_name: &str,
        tool_call_id: Option<&str>,
        tool_input: Option<&serde_json::Value>,
        tool_output: Option<&str>,
        status: Option<&str>,
        duration_ms: Option<u64>,
        timestamp: i64,
    ) -> String { ... }

    /// Append a human gate node (permission requested).
    pub fn append_human_gate(&mut self, reason: &str, timestamp: i64) -> String { ... }

    /// Append a patch proposal node (change recorded).
    pub fn append_patch_proposal(
        &mut self,
        change_hash: &str,
        files: &[String],
        timestamp: i64,
    ) -> String { ... }

    /// Mark a human gate as resolved.
    pub fn resolve_human_gate(&mut self, gate_id: &str) { ... }

    /// Persist the graph to disk.
    pub fn save(&self, session_dir: &Path) -> AgentResult<()> { ... }

    /// Serialize to a compact text summary suitable for LLM compaction context.
    pub fn to_compaction_summary(&self) -> String { ... }

    /// Serialize to the full JSON representation.
    pub fn to_serialized_graph(&self) -> SerializedGraph { ... }

    /// Get aggregate statistics.
    pub fn stats(&self) -> GraphStats { ... }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize { ... }

    // ---- Private: edge inference ----

    /// Infer and add edges when a new node is appended.
    fn infer_edges(&mut self, node_id: &str, kind: NodeKind) { ... }
}
```

**Edge inference rules (implemented in `infer_edges`):**

| New node kind | Edge(s) created | Logic |
|---------------|----------------|-------|
| `Goal` | `prev_goal --led_to-→ new_goal` (if chained prompts) | Goals chain sequentially |
| `Exploration` | `current_goal --led_to-→ exploration` | Exploration serves the current goal |
| `Commitment` | `exploration --explored_via-→ commitment` for each pending exploration; `current_goal --led_to-→ commitment` if no explorations pending | Commitment is informed by preceding explorations |
| `Verification` | `last_commitment --verified_by-→ verification` | Verification validates the most recent commitment |
| `Execution` | `current_goal --led_to-→ execution` | Execution serves the current goal |
| `Error` | `last_node --failed_with-→ error` | Error caused by whatever preceded it |
| `HumanGate` | `last_node --blocked_by-→ gate` | Gate blocks the current activity |
| `PatchProposal` | `last_commitment --committed_via-→ patch` (for each commitment in the current goal's chain) | Patch proposal links to all commitments it contains |

After creating `Commitment` edges, pending explorations are cleared. After
creating a `Goal`, pending explorations and last commitment are reset.

### 3.5 Integration with TurnOrchestrator

The orchestrator gains a `ProvenanceAccumulator` field and calls into it at
each dispatch point. The changes are minimal and surgical:

**`handle_session_start`** — Create or load the accumulator:
```rust
// In handle_session_start, after creating/loading the session:
let accumulator = ProvenanceAccumulator::load_or_create(
    &self.session_dir(&session.session_id),
    &session.session_id,
)?;
// Store in orchestrator or pass to subsequent handlers
```

**`handle_turn_start`** — Append a goal node from the user prompt:
```rust
// In handle_turn_start, after storing the prompt:
if let Some(ref prompt) = event.prompt {
    let mut acc = self.load_accumulator(&session.session_id)?;
    acc.append_goal(prompt, event.timestamp.timestamp_millis());
    acc.save(&self.session_dir(&session.session_id))?;
}
```

**`handle_tool_use`** — This is the big one. Replace the log-and-return with
graph accumulation:
```rust
// In handle_tool_use, replacing the current "Log tool usage" block:
let mut acc = self.load_accumulator(&session.session_id)?;

if event.event_type == HookType::PostToolUse {
    // Extract classification data from raw_json
    let raw = event.raw_json.as_ref();
    let tool_input = raw.and_then(|j| j.get("tool_input"));
    let tool_output = raw.and_then(|j| j.get("tool_output")).and_then(|v| v.as_str());
    let status = raw.and_then(|j| j.get("status")).and_then(|v| v.as_str());
    let duration = raw.and_then(|j| j.get("duration")).and_then(|v| v.as_u64());

    acc.append_tool_call(
        event.tool_name.as_deref().unwrap_or("unknown"),
        event.tool_use_id.as_deref(),
        tool_input,
        tool_output,
        status,
        duration,
        event.timestamp.timestamp_millis(),
    );
    acc.save(&self.session_dir(&session.session_id))?;
}
// PreToolUse is currently a no-op for the graph — we only need the
// result to classify. But we store the tool_call_id for correlation.
```

**`handle_turn_end`** — After recording a change, append a patch proposal:
```rust
// In handle_turn_end, after a successful record_turn():
if let Ok(ref outcome) = record_result {
    let mut acc = self.load_accumulator(&session.session_id)?;
    acc.append_patch_proposal(
        &outcome.hash.to_base32(),
        &outcome.recorded_file_list(),
        chrono::Utc::now().timestamp_millis(),
    );
    acc.save(&self.session_dir(&session.session_id))?;
}
```

**`handle_session_end`** — Finalize and (in Phase 2) save as content-addressed artifact:
```rust
// In handle_session_end, before creating the attestation:
if let Ok(acc) = self.load_accumulator(&session.session_id) {
    if acc.node_count() > 0 {
        log::info!(
            "Session {} provenance graph: {} nodes, {} edges",
            session.session_id,
            acc.node_count(),
            acc.stats().edge_count,
        );
        // Phase 2: save_provenance_artifact(&repo, &acc, &session);
    }
}
```

### 3.6 Persistence format

The accumulator saves to `.atomic/sessions/{session_id}/graph.json`:

```json
{
  "version": 1,
  "session_id": "abc-123-def-456",
  "nodes": [
    {
      "id": "abc-123-def-456-1",
      "kind": "goal",
      "timestamp": 1735689600000,
      "summary": "Fix the auth bug in login.rs",
      "detail": null,
      "tool_name": null,
      "classified": false
    },
    {
      "id": "abc-123-def-456-2",
      "kind": "exploration",
      "timestamp": 1735689601200,
      "summary": "Read src/auth/login.rs",
      "tool_name": "read",
      "tool_call_id": "call-42"
    },
    {
      "id": "abc-123-def-456-3",
      "kind": "commitment",
      "timestamp": 1735689605500,
      "summary": "Edit src/auth/login.rs",
      "tool_name": "edit",
      "tool_call_id": "call-43",
      "detail": { "files_modified": ["src/auth/login.rs"] }
    }
  ],
  "edges": [
    { "from": "abc-123-def-456-1", "to": "abc-123-def-456-2", "kind": "led_to" },
    { "from": "abc-123-def-456-2", "to": "abc-123-def-456-3", "kind": "explored_via" }
  ],
  "stats": {
    "goal_count": 1,
    "exploration_count": 1,
    "commitment_count": 1,
    "edge_count": 2
  },
  "counter": 3
}
```

This file is read/written on every hook invocation. Typical size for a 20-tool
session: ~5-10 KB. Read + deserialize + append + serialize + write is well under
10ms.

### 3.7 Compaction summary format

The accumulator produces a compact text summary for the compaction hook:

```
## Session Provenance (12 nodes)

### Goals
- [1] Fix the auth bug in login.rs
- [8] Add tests for the token validation fix

### Decision Chain
1. Goal: Fix the auth bug in login.rs
   ├── Explored: src/auth/login.rs, src/auth/jwt.rs, src/middleware.rs (3 files)
   ├── Committed: src/auth/login.rs (fixed timezone comparison)
   └── Verified: cargo test --lib (passed)
2. Goal: Add tests for the token validation fix
   ├── Explored: tests/auth_test.rs
   ├── Committed: tests/auth_test.rs (added 3 test cases)
   └── Verified: cargo test tests::auth (passed)

### Patches
- Change ABCD1234: src/auth/login.rs (+12 -3)
- Change EFGH5678: tests/auth_test.rs (+45 -0)

### Open
- Human gate (pending): "Should I also fix the refresh endpoint?"
```

This is optimized for LLM consumption — structured enough to be useful,
concise enough to fit in a compaction context budget (~200-400 tokens for
a typical session).

### 3.8 Testing strategy

All new modules are pure Rust with no filesystem or repository dependencies
in the core logic. The accumulator's persistence layer is the only I/O, and
it's behind a thin save/load boundary.

| Module | Test focus |
|--------|-----------|
| `types.rs` | Serde round-trip for all node/edge kinds |
| `classify.rs` | Tool name → NodeKind mapping, shell command sub-classification |
| `accumulator.rs` | Append operations, edge inference correctness, DAG invariants (no cycles), stats accuracy |
| `serialize.rs` | JSON round-trip, compaction text output format |
| Integration | Mock a realistic session (prompt → 3 reads → 1 edit → test → idle) through the accumulator and verify graph structure |

```rust
#[test]
fn test_typical_session_graph_structure() {
    let mut acc = ProvenanceAccumulator::new("test-session");

    // Human asks to fix a bug
    let goal = acc.append_goal("Fix the auth bug", 1000);

    // Agent reads 3 files
    let r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
    let r2 = acc.append_tool_call("read", Some("c2"), None, None, None, None, 1002);
    let r3 = acc.append_tool_call("read", Some("c3"), None, None, None, None, 1003);

    // Agent edits one file
    let edit = acc.append_tool_call("edit", Some("c4"), None, None, None, None, 1004);

    // Agent runs tests
    let test_input = serde_json::json!({"command": "cargo test"});
    let test = acc.append_tool_call("bash", Some("c5"), Some(&test_input), None, None, None, 1005);

    // Verify graph structure
    assert_eq!(acc.node_count(), 6);

    let stats = acc.stats();
    assert_eq!(stats.goal_count, 1);
    assert_eq!(stats.exploration_count, 3);
    assert_eq!(stats.commitment_count, 1);
    assert_eq!(stats.verification_count, 1);

    // Verify edges: goal → explorations, explorations → commitment, commitment → verification
    let edges = acc.edges();
    assert!(edges.iter().any(|e| e.from == goal && e.to == r1 && e.kind == EdgeKind::LedTo));
    assert!(edges.iter().any(|e| e.from == r1 && e.to == edit && e.kind == EdgeKind::ExploredVia));
    assert!(edges.iter().any(|e| e.from == edit && e.to == test && e.kind == EdgeKind::VerifiedBy));
}
```

### 3.9 Deliverables

- [ ] `atomic-agent/src/provenance/mod.rs` — Module root + re-exports
- [ ] `atomic-agent/src/provenance/types.rs` — `NodeKind`, `EdgeKind`, `GraphNode`, `GraphEdge`, `GraphStats`
- [ ] `atomic-agent/src/provenance/classify.rs` — `classify_tool_call()`, shell command sub-classifiers
- [ ] `atomic-agent/src/provenance/accumulator.rs` — `ProvenanceAccumulator` with append, edge inference, save/load
- [ ] `atomic-agent/src/provenance/serialize.rs` — `SerializedGraph`, JSON output, compaction text output
- [ ] Updated `TurnOrchestrator`:
  - [ ] `handle_turn_start` → append goal node
  - [ ] `handle_tool_use` → classify + append tool node (PostToolUse only)
  - [ ] `handle_turn_end` → append patch proposal after successful record
  - [ ] `handle_session_end` → log final graph stats
- [ ] Unit tests for classify, accumulator (edge inference), serialization round-trips
- [ ] Integration test: full session lifecycle through orchestrator verifying graph output

---

## 4. Phase 2 — Provenance Storage Schema (Rust, atomic-core)

> **Duration**: ~2 weeks
> **Scope**: Rust, `atomic-core` + `atomic-repository` + `atomic-agent`
> **Dependency**: Phase 1 (graph types and accumulator must be stable)

### 4.1 Design decision: new content-addressed type

The provenance graph follows the same pattern as `Attestation`:

| Property | Attestation | Provenance Graph |
|----------|-------------|------------------|
| Magic bytes | `ATST` | `PRVG` |
| File extension | `.attest` | `.provenance` |
| Node type in `NODE_TYPES` | `2` | `3` |
| Serialization | postcard | postcard |
| Content-addressed | Yes (Blake3) | Yes (Blake3) |
| Dependencies | → covered changes | → explained changes |
| Stack membership | No (`STACK_CHANGES`) | No |
| Push/pull | Yes (travels with changes) | Yes |

**Why not embed in the change's unhashed section?**

The unhashed section (where transcripts live today) is per-change. The
provenance graph spans multiple changes within a session. It needs its own
identity so it can be:
- Queried across sessions without loading every change
- Linked to from the WebUI by hash
- Chained across resumed sessions (like attestation chaining)

**Why not extend Attestation?**

Attestations are flat summaries (cost, tokens, duration). The provenance graph
is a DAG with typed nodes and edges. They serve different purposes:
- Attestation: "How much did this session cost?"
- Provenance: "Why did the agent make this change?"

They can reference each other via deps.

### 4.2 Rust types

```rust
// atomic-core/src/change/provenance.rs

/// Magic bytes identifying a provenance graph file: "PRVG"
const MAGIC: &[u8; 4] = b"PRVG";

/// Current schema version.
const SCHEMA_VERSION: u8 = 1;

/// File extension for provenance graph files.
pub const PROVENANCE_EXTENSION: &str = "provenance";

/// A content-addressed provenance graph for an agent session segment.
///
/// Captures the causal decision chain that produced a set of changes.
/// Stored alongside change files, registered in the graph with
/// node_type = PROVENANCE (3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceGraph {
    pub version: u8,
    pub timestamp: i64,
    pub session_id: String,
    pub agent_name: String,
    pub agent_display_name: String,
    pub agent_vendor: String,

    /// The typed nodes in the decision DAG.
    pub nodes: Vec<ProvenanceNode>,

    /// Causal edges between nodes.
    pub edges: Vec<ProvenanceEdge>,

    /// Hashes of the changes this graph explains.
    /// Also registered in the DEPS table.
    pub changes_explained: Vec<Hash>,

    /// Previous provenance graph in this session (for resume chaining).
    #[serde(default)]
    pub previous: Option<Hash>,

    /// Aggregate statistics.
    pub stats: ProvenanceStats,
}
```

Note: `ProvenanceNode`, `ProvenanceEdge`, and `ProvenanceStats` are structurally
identical to the types in `atomic-agent/src/provenance/types.rs` but use
`atomic-core` serialization conventions (postcard, `Hash` type instead of
`String`). The accumulator's `SerializedGraph` converts to this type for
storage.

### 4.3 Storage integration

Add to `atomic-core/src/pristine/tables.rs`:

```rust
pub mod node_type {
    pub const CHANGE: u8 = 0;
    pub const TAG: u8 = 1;
    pub const ATTESTATION: u8 = 2;
    pub const PROVENANCE: u8 = 3;   // NEW
}
```

Add to `MutTxnT`:

```rust
fn register_provenance(&mut self, hash: &Hash) -> PristineResult<NodeId>;
```

Implementation follows `register_attestation` exactly: allocate ID, insert
into EXTERNAL + INTERNAL + NODE_TYPES.

Add to `ChangeStore`:

```rust
fn save_provenance_graph(&self, graph: &ProvenanceGraph) -> Result<Hash>;
fn load_provenance_graph(&self, hash: &Hash) -> Result<Option<ProvenanceGraph>>;
fn iter_provenance_graphs(&self) -> impl Iterator<Item = Result<(Hash, ProvenanceGraph)>>;
```

Add to `Repository`:

```rust
fn save_provenance_graph(&self, graph: &ProvenanceGraph) -> Result<Hash>;
fn find_provenance_for_change(&self, change_hash: &Hash) -> Result<Vec<(Hash, ProvenanceGraph)>>;
fn find_provenance_for_session(&self, session_id: &str) -> Result<Vec<(Hash, ProvenanceGraph)>>;
```

### 4.4 Saving provenance graphs from the orchestrator

On **turn recording** (not session end), the orchestrator converts the
accumulator's current state to a `ProvenanceGraph` artifact covering the
changes recorded in this turn:

```rust
// In handle_turn_end, after successful record_turn():
let acc = self.load_accumulator(&session.session_id)?;
let provenance = acc.to_provenance_graph(
    &session,
    &[outcome.hash],  // changes_explained = this turn's change
);

match repo.save_provenance_graph(&provenance) {
    Ok(hash) => {
        log::info!("Saved provenance graph {} for turn {}", hash.to_base32(), turn_number);
    }
    Err(e) => {
        log::warn!("Failed to save provenance graph (non-fatal): {}", e);
    }
}
```

On **session end**, a final provenance graph is saved covering any changes
not yet covered by a per-turn graph (edge case: session ends without a
clean turn end). This graph chains to the per-turn graphs via `previous`.

### 4.5 Cross-session graph merging

Handled at **query time**, not storage time:

1. Each session produces one or more `ProvenanceGraph` artifacts (one per
   turn recording, chained via `previous`).
2. The WebUI queries all provenance graphs for a project/stack.
3. Graphs are joined by shared `changes_explained` references — if session A
   produced change X, and session B depends on change X, the graphs connect
   at that change node.
4. Goals from different sessions are top-level roots in the project DAG.

No special merge operation needed. The graph is append-only and conflict-free
by construction (content-addressed, per-session).

### 4.6 Deliverables

- [ ] `atomic-core/src/change/provenance.rs` — `ProvenanceGraph`, serialize/deserialize
- [ ] `node_type::PROVENANCE = 3` in tables.rs
- [ ] `register_provenance()` in `MutTxnT`
- [ ] `save_provenance_graph()` / `load_provenance_graph()` in ChangeStore
- [ ] `save_provenance_graph()` / `find_provenance_*()` in Repository
- [ ] Accumulator `to_provenance_graph()` conversion method
- [ ] Orchestrator saves provenance graph on turn recording
- [ ] Unit tests: round-trip serialization, node type registration, query by change/session

---

## 5. Phase 3 — Classification Layer

> **Duration**: ~1 week
> **Scope**: Rust, `atomic-agent`
> **Dependency**: Phase 1 (graph accumulator must exist)

### 5.1 What classification does

The rule-based classifier from Phase 1 produces one node per tool call. A
20-tool-call turn produces 20 nodes. That's too noisy for the WebUI.

The classification layer consolidates sequences of raw tool events into
named **decision nodes**:

```
Before classification:
  exploration: read auth.rs
  exploration: read jwt.rs
  exploration: read middleware.rs
  commitment: edit auth.rs
  verification: cargo test

After classification:
  decision: "Explored auth module → identified timezone bug → fixed token validation"
    └─ consolidated_from: [node-2, node-3, node-4, node-5, node-6]
```

### 5.2 Two-tier classification

**Tier 1: Pattern-based consolidation (no LLM, runs on every turn end)**

Runs synchronously in the orchestrator. Groups consecutive tool calls of the
same kind into sequences:
- 3 consecutive reads → single "Explored 3 files in src/auth/" decision node
- Read + edit + test → "Implemented and verified change to {file}" decision sequence

This is deterministic and fast. It catches the 80% case.

```rust
// atomic-agent/src/provenance/classify.rs

/// Consolidate raw tool nodes into decision nodes.
///
/// Scans the graph for sequences of consecutive same-kind nodes and
/// replaces them with a single decision node that references the originals.
pub fn consolidate_sequences(acc: &mut ProvenanceAccumulator) {
    // Group consecutive explorations
    // Group read → edit → test into "implement and verify" decisions
    // Detect backtracking (read → edit → read same file → edit same file)
}
```

**Tier 2: LLM-powered naming (async, runs via `atomic agent explain`)**

The existing `explain` command already generates AI reasoning summaries from
transcripts. Extend it to consume the provenance graph:

```rust
// In atomic-agent explain workflow:
// 1. Load the provenance graph for the session
// 2. Pass the graph structure to the LLM (much richer than raw transcript)
// 3. LLM generates named decision summaries + confidence scores
// 4. Update graph nodes with classified=true, confidence, summary
// 5. Save updated provenance graph
```

This reuses the existing `explain` infrastructure. No new LLM integration
needed — just a richer input to the same pipeline.

### 5.3 Backtracking detection

A key signal for decision quality is **backtracking** — when the agent reads a
file, edits it, then reads again and edits again:

- Read → edit → read same file → edit same file = backtracking
- Edit → test (fail) → edit same file = test-driven iteration
- Read → read → read (same directory) = systematic exploration

These patterns are detected during Tier 1 consolidation and surfaced in the
decision node's detail as `alternatives` or `iterations`.

### 5.4 Deliverables

- [ ] `consolidate_sequences()` in classify.rs — Tier 1 pattern consolidation
- [ ] Backtracking detection logic
- [ ] Integration with `handle_turn_end` — run consolidation after recording
- [ ] Extended `atomic agent explain` to consume provenance graph as input
- [ ] Tests for sequence consolidation, backtracking detection

---

## 6. Phase 4 — Compaction Hook (Plugin)

> **Duration**: ~1–2 days
> **Scope**: TypeScript, `opencode/.opencode/plugins/atomic/`
> **Dependency**: Phase 1 (graph must be persisted to disk)

### 6.1 Why compaction matters

OpenCode compacts the conversation when the context window fills up. Without
intervention, all tool call history and reasoning context is lost. The LLM
starts fresh with a summary.

The compaction hook reads the provenance graph from disk and injects a
structured summary into the compacted context. This is the moat — every
other tool loses its reasoning history at compaction. You preserve it.

### 6.2 Implementation

This is the **only** new plugin-side code. It's trivially thin because the
graph lives on disk (maintained by the Rust side):

```typescript
// In .opencode/plugins/atomic/handlers/compaction.ts

import { readFile } from "fs/promises"
import { join } from "path"

export function createCompactionHandler(directory: string) {
  return async (
    input: { sessionID: string },
    output: { context: string[]; prompt?: string }
  ) => {
    // Read the graph from .atomic/sessions/{id}/graph.json
    const graphPath = join(directory, ".atomic", "sessions", input.sessionID, "graph.json")

    try {
      const raw = await readFile(graphPath, "utf-8")
      const graph = JSON.parse(raw)
      const summary = formatCompactionSummary(graph)
      if (summary) output.context.push(summary)
    } catch {
      // Graph doesn't exist yet or can't be read — that's fine, skip
    }
  }
}

function formatCompactionSummary(graph: any): string | null {
  if (!graph.nodes || graph.nodes.length === 0) return null

  const goals = graph.nodes.filter((n: any) => n.kind === "goal")
  const decisions = graph.nodes.filter((n: any) => n.kind === "decision")
  const commitments = graph.nodes.filter((n: any) => n.kind === "commitment")
  const gates = graph.nodes.filter((n: any) => n.kind === "human_gate")
  const patches = graph.nodes.filter((n: any) => n.kind === "patch_proposal")

  const lines = [
    `## Session Provenance (${graph.nodes.length} nodes)`,
    "",
    "### Goals",
    ...goals.map((n: any) => `- ${n.summary}`),
  ]

  if (decisions.length > 0) {
    lines.push("", "### Decisions")
    for (const d of decisions) lines.push(`- ${d.summary}`)
  }

  if (commitments.length > 0) {
    lines.push("", "### Files Changed")
    for (const c of commitments) lines.push(`- ${c.summary}`)
  }

  if (patches.length > 0) {
    lines.push("", "### Recorded Changes")
    for (const p of patches) lines.push(`- ${p.change_hash}: ${p.summary}`)
  }

  if (gates.length > 0) {
    lines.push("", "### Human Gates")
    for (const g of gates) lines.push(`- ${g.summary}`)
  }

  return lines.join("\n")
}
```

Then wire it into the plugin entry point:

```typescript
// In index.ts, add to the returned hooks object:
"experimental.session.compacting": createCompactionHandler(directory),
```

### 6.3 Alternative: CLI-based compaction

If reading the file directly feels too coupled, the plugin can shell out to
a new CLI command instead:

```bash
atomic agent graph summary <session-id> --format compaction
```

This keeps the plugin purely as a CLI pipe (consistent with existing design),
at the cost of one process spawn during compaction. Given that compaction is
infrequent (every ~30 minutes of active use), the latency is acceptable.

### 6.4 Deliverables

- [ ] `handlers/compaction.ts` — Read graph from disk, format summary
- [ ] Updated `index.ts` — Wire compaction handler
- [ ] OR: `atomic agent graph summary` CLI command (if preferring CLI approach)
- [ ] Test: verify summary is under 500 tokens for a 20-node graph

---

## 7. Phase 5 — WebUI

> **Duration**: ~3 weeks
> **Scope**: React (the-hive), Rust (atomic-api)
> **Dependency**: Phase 2 (provenance graphs must be stored and queryable)

### 7.1 API endpoints

Add to `atomic-enterprise/atomic-api`:

```
GET /api/v1/projects/:slug/provenance
    → List all provenance graphs for a project
    Query params: session_id, stack, since, limit

GET /api/v1/projects/:slug/provenance/:hash
    → Single provenance graph by content hash

GET /api/v1/projects/:slug/provenance/by-change/:changeHash
    → Provenance graphs explaining a specific change

GET /api/v1/projects/:slug/provenance/merged
    → Project-level merged graph (all sessions joined at shared changes)
    Query params: since, stack, session_ids
```

### 7.2 UI architecture

New route in `the-hive/apps/web`:

```
/tenants/:tenantSlug/portfolios/:portfolioSlug/projects/:projectSlug/provenance
```

Three-panel layout:

#### Left panel — Intent Graph

- Interactive DAG visualization using **React Flow** (MIT license, handles
  large graphs, supports custom nodes and edge types).
- Nodes are color-coded by kind:
  - Blue: goal
  - Yellow: decision
  - Green: commitment / patch proposal
  - Red: human gate / error
  - Gray: exploration, verification, execution
- Edges show causality direction (animated flow for active sessions).
- Collapsible subtrees — collapse an entire exploration sequence into a
  single "explored 5 files" node.
- Zoom + pan + minimap for large graphs.
- Session boundaries shown as dashed regions.

#### Center panel — Node Inspector

- Clicking any node in the DAG opens its detail view.
- **Goal node**: The human prompt, what constraints it implies, downstream
  decision count.
- **Decision node**: Tool calls that composed it, what alternatives were
  considered (backtracking), which commitment it produced, the classifier's
  confidence score.
- **Commitment node**: The actual diff (rendered inline), the causal chain
  that motivated it. "Why did this change happen?" is answered by the edges.
- **Patch proposal node**: Full diff + the causal chain. Deep link to the
  Atomic change detail page.
- **Human gate**: The permission request, the response, how long the agent
  was blocked.

#### Right panel — Session Timeline

- Chronological event stream. Secondary — for debugging the graph when
  something looks wrong.
- Shows raw tool calls with timestamps, durations, and their classification.
- Selecting an event in the timeline highlights the corresponding node in
  the DAG.
- Filterable by event type, session, time range.

### 7.3 The critical interaction — backward traversal

The WebUI's killer feature is not the visualization itself but the
**traversal**:

1. User sees a bug in production.
2. They navigate to the file in the project view and find the change that
   introduced it (via Atomic blame).
3. They click "Show Provenance" on the change.
4. The DAG highlights the **patch node** for that change.
5. They follow the edge backward to the **commitment node** — they see the
   exact edit and the tool call that made it.
6. They follow the edge backward to the **decision node** — they see the
   strategy the agent chose and whether it considered alternatives.
7. They follow the edge backward to the **goal node** — they see the human
   prompt that started the whole chain.

Total time: 10 seconds. In GitHub: an hour of `git blame` and commit
archaeology.

### 7.4 Deliverables

- [ ] API endpoints in `atomic-api` for provenance CRUD
- [ ] React Flow-based DAG component with custom node renderers
- [ ] Node Inspector panel with kind-specific detail views
- [ ] Session Timeline panel (filterable chronological stream)
- [ ] Backward traversal from change → provenance graph → highlighted path
- [ ] Route integration in `the-hive` app

---

## 8. Cross-Cutting Concerns

### 8.1 Privacy and redaction

Provenance graphs may contain prompt text and tool call arguments that include
sensitive information. Follow the same pattern as transcripts:

**Split storage**: The graph skeleton (node kinds, summaries, edges, stats) is
**hashed** and tamper-evident. The `detail` field on each node (raw tool args,
outputs) is **unhashed** and redactable. This lets you strip raw detail without
invalidating the graph's content hash.

### 8.2 Performance budget

| Constraint | Budget |
|-----------|--------|
| Graph load from disk (JSON) | < 5ms for 100-node graph |
| Classify tool call (rule-based) | < 0.1ms |
| Append node + infer edges | < 1ms |
| Save graph to disk (JSON) | < 5ms for 100-node graph |
| Consolidate sequences (Tier 1) | < 10ms for 100 nodes |
| Save ProvenanceGraph artifact (postcard + Blake3) | < 50ms |
| Compaction summary generation | < 5ms |
| WebUI: initial DAG render (100 nodes) | < 500ms |
| WebUI: backward traversal query | < 200ms |

The dominant cost is the process spawn for each hook invocation (the existing
overhead), not the graph operations themselves. Graph work adds ~10ms per hook
call on top of the existing ~50-200ms for process startup + repo open + save.

### 8.3 Graph size limits

For very long sessions (hundreds of tool calls), the raw graph gets large.
Mitigations:

1. **Classification consolidation** (Phase 3) reduces node count 5-10x.
2. **Per-turn snapshots**: Save a provenance graph artifact at each turn
   recording, not just at session end. Each covers the incremental subgraph
   since the last save. Chain via `previous`.
3. **Compaction summarization**: The compaction hook only injects the
   classified/consolidated view, not the raw tool events.
4. **WebUI lazy loading**: Load the graph skeleton first, then fetch node
   detail on click.

### 8.4 Failure modes

The provenance graph is **best-effort**. Failures must never block the agent:

| Failure | Impact | Mitigation |
|---------|--------|------------|
| Graph load/save fails | Lost node | try/catch, log, continue — turn recording unaffected |
| Classification fails | No consolidated summary | Raw nodes stand as-is |
| Compaction read fails | Lost context across compaction | Log warning, session continues |
| ProvenanceGraph artifact save fails | Graph not content-addressed | Log warning, JSON on disk is still the source of truth |
| Graph JSON corrupted (crash mid-write) | Session graph lost | Atomic write (temp file + rename), or rebuild from attestation/change metadata |

### 8.5 Backwards compatibility

- The orchestrator falls back gracefully if no graph file exists (older
  sessions, manual recordings without agent hooks).
- The new `node_type::PROVENANCE = 3` is unknown to older Atomic versions.
  They ignore it in graph traversal (same as how `ATTESTATION` was added).
- `.provenance` files are ignored by older versions of the change store
  (they only look for `.change` and `.attest` files).
- All orchestrator graph operations are wrapped in `if let Ok(...)` — a
  missing or corrupt graph never prevents turn recording or session management.

### 8.6 Agent compatibility matrix

Because the graph accumulator lives in `TurnOrchestrator`, it works for
every agent that flows through the hook system:

| Agent | SessionStart | TurnStart (goal) | ToolUse (nodes) | TurnEnd (patch) | Graph? |
|-------|-------------|-------------------|-----------------|-----------------|--------|
| Claude Code | ✅ | ✅ (prompt) | ✅ (PreToolUse + PostToolUse) | ✅ | **Full** |
| Gemini CLI | ✅ | ✅ (prompt) | ⚠️ (PostToolUse only) | ✅ | **Partial** (no pre-tool timing) |
| Codex | ✅ | ✅ (prompt) | ✅ | ✅ | **Full** |
| OpenCode | ✅ | ✅ (prompt) | ✅ (before + after) | ✅ | **Full** |

Agents that don't send tool use hooks still get goal → patch_proposal edges.
The graph is thinner but still useful for backward traversal from change to
human intent.

---

## 9. Open Questions

### 9.1 Should decision nodes be mutable?

The classification layer runs on turn end, but the agent might continue
working in the next turn. Should decision nodes be updated with new
information, or should a new decision node be created?

**Proposed answer**: Decision nodes are immutable once created. If new tool
calls arrive in a subsequent turn, they form a new sequence that will be
classified separately. The graph is append-only. Decision nodes from
different turns can be linked via `led_to` edges if they serve the same goal.

### 9.2 How do we handle multi-agent sessions?

If two agents work on the same project concurrently, each has its own session
and its own provenance graph. The project-level view merges them at shared
change dependencies.

For OpenCode's sub-agent model (where one agent spawns another via the Task
tool), the parent session's graph creates an `Execution` node for the Task
tool call. If the sub-agent has its own session, the graphs link via the
change dependencies. If not, the Task tool's output is captured in the
parent node's detail.

### 9.3 Graph schema versioning

The `version: 1` field in `ProvenanceGraph` allows forward-compatible schema
evolution:

- Adding new `NodeKind` or `EdgeKind` variants: No version bump (unknown
  variants render as "unknown" in the WebUI).
- Adding new fields to `GraphNode`: No version bump (serde default).
- Changing the meaning of existing fields: Version bump.
- Changing serialization format: Version bump.

### 9.4 Integration with `explain`

The existing `atomic agent explain` command generates AI reasoning summaries
for recorded turns. Once provenance graphs are available, `explain` should use
the graph as primary input — it's a much richer signal than raw transcript.
The graph tells you *what the agent was trying to do*; the transcript tells
you *what it said while doing it*. Use both.

### 9.5 Real-time streaming to WebUI

Should the WebUI show the provenance graph building in real-time during an
active session?

**Proposed answer**: Defer to a later phase. Start with post-session
visualization. Real-time adds significant complexity (partial graph states,
animation, WebSocket connections) for marginal value in V1. The graph file on
disk could be polled, but the latency/complexity tradeoff isn't worth it yet.

### 9.6 Session directory structure

Currently sessions are stored as flat JSON files:
```
.atomic/sessions/2026-01-15-abc123de.json
```

The provenance graph needs a sibling file. Two options:

**Option A**: Session directory (recommended):
```
.atomic/sessions/2026-01-15-abc123de/
├── session.json       ← existing AgentSession (renamed from flat file)
└── graph.json         ← provenance graph
```

**Option B**: Sibling files:
```
.atomic/sessions/2026-01-15-abc123de.json         ← existing
.atomic/sessions/2026-01-15-abc123de.graph.json   ← new
```

Option A is cleaner and leaves room for future per-session artifacts (e.g.,
tool call logs, sub-agent graphs). The migration from flat file to directory
is straightforward — `SessionStore::load` tries the directory first, falls
back to the flat file, and migrates on next save.

---

## 10. Appendix — Existing Infrastructure Map

### What exists and where it lives

| Component | Location | Relevant files |
|-----------|----------|---------------|
| OpenCode plugin | `opencode/.opencode/plugins/atomic/` | `index.ts`, `handlers/`, `session.ts`, `types.ts` |
| Plugin API types | `opencode/packages/plugin/src/index.ts` | `Hooks` interface with all event signatures |
| Agent hooks (Rust) | `atomic/atomic-agent/src/hooks/opencode.rs` | `OpenCodeHook`, payload parsing, `AfterToolInput` |
| Turn orchestrator | `atomic/atomic-agent/src/turn/orchestrator.rs` | `TurnOrchestrator`, `dispatch()`, `handle_tool_use()` |
| Turn recording | `atomic/atomic-agent/src/record.rs` | `record_turn()`, `TurnRecordOutcome` |
| Session state | `atomic/atomic-agent/src/turn/session.rs` | `AgentSession`, `SessionStore` |
| Phase state machine | `atomic/atomic-agent/src/turn/phase.rs` | `Phase`, `Event`, `Action`, `transition()` |
| Attestation model | `atomic/atomic-core/src/change/attestation.rs` | `Attestation`, `AttestAgent`, serialize/deserialize |
| Transcript/reasoning | `atomic/atomic-agent/src/transcript.rs` | `UnhashedTurnData`, `TurnReasoning`, `Learnings` |
| Node type registry | `atomic/atomic-core/src/pristine/tables.rs` | `node_type::CHANGE/TAG/ATTESTATION` |
| Traceability | `atomic-enterprise/atomic-sessions/src/traceability.rs` | `TraceabilityInfo` |
| Server API | `atomic-enterprise/atomic-api/` | HTTP API for remote operations |
| Web UI | `the-hive/apps/web/` | React + Tailwind + Vite, Operations Room exists |

### Existing hooks used by the orchestrator

| Hook type | Orchestrator method | Current behavior | New behavior (Phase 1) |
|-----------|-------------------|------------------|----------------------|
| `SessionStart` | `handle_session_start` | Create/resume session, fork stack | + Load/create `ProvenanceAccumulator` |
| `TurnStart` | `handle_turn_start` | Store prompt, begin watcher, transition phase | + Append goal node from prompt |
| `PreToolUse` | `handle_tool_use` | Log tool name | No change (correlate with PostToolUse via tool_call_id) |
| `PostToolUse` | `handle_tool_use` | Log tool name | + Classify tool call, append typed node, infer edges |
| `TurnEnd` | `handle_turn_end` | Record change, transition phase | + Append patch proposal, save provenance artifact |
| `SessionEnd` | `handle_session_end` | Transition phase, create attestation | + Log graph stats, finalize graph |

### Existing node types in Atomic

| Type | Value | File extension | Purpose |
|------|-------|---------------|---------|
| Change | 0 | `.change` | Content changes (hunks, edges) |
| Tag | 1 | `.tag` | Named state snapshots |
| Attestation | 2 | `.attest` | Session cost/token summaries |
| **Provenance** | **3** | **`.provenance`** | **Decision DAG (new)** |

### Data already available in TurnEvent for classification

| Field | Available for | Used by classifier |
|-------|--------------|-------------------|
| `tool_name` | PreToolUse, PostToolUse | Primary: determines NodeKind |
| `tool_use_id` | PreToolUse, PostToolUse | Correlate pre/post pairs |
| `raw_json.tool_input` | PostToolUse (OpenCode) | Shell command sub-classification |
| `raw_json.tool_output` | PostToolUse (OpenCode) | Error detection, test pass/fail |
| `raw_json.status` | PostToolUse (OpenCode) | "error" → Error node |
| `raw_json.duration` | PostToolUse (OpenCode) | Tool timing metadata |
| `raw_json.modified_files` | PostToolUse (OpenCode) | Confirm commitment classification |
| `prompt` | TurnStart | Goal node content |
| `timestamp` | All events | Node ordering, edge inference |

---

## Summary — Build order

| Phase | What | Where | Duration | Depends on |
|-------|------|-------|----------|------------|
| **1** | Graph accumulator + types + classifier + persistence in orchestrator | `atomic-agent` (Rust) | 2 weeks | Nothing |
| **2** | `ProvenanceGraph` content-addressed storage schema | `atomic-core` + `atomic-repository` (Rust) | 2 weeks | Phase 1 |
| **3** | Sequence consolidation + backtracking detection + explain integration | `atomic-agent` (Rust) | 1 week | Phase 1 |
| **4** | Compaction hook (read graph from disk, inject summary) | Plugin (TS) — tiny | 1–2 days | Phase 1 |
| **5** | WebUI (DAG + inspector + timeline + backward traversal) | `the-hive` (React) + `atomic-api` (Rust) | 3 weeks | Phase 2 |

**Phase 1 is the wedge.** It starts accumulating provenance data immediately
for every agent (not just OpenCode), persists it durably alongside session
state, and is fully testable in isolation with Rust unit tests. Everything else
builds on having a graph to classify, persist, compress, and visualize.