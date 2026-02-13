# Attestation Design Spec

## Overview

Attestations are a new graph-level node type that captures audit and compliance
metadata about a set of changes — cost, token usage, model breakdown, duration,
and agent identity. Unlike changes and tags, attestations are **not** part of any
stack's changelog. They live in the graph as standalone nodes with dependencies
pointing to the changes they cover.

This design enables:
- Full audit trail that survives stack deletion
- Cross-stack visibility (one attestation can cover changes across multiple stacks)
- Compliance queries ("what's the AI cost for everything in release?")
- Gap detection ("which changes have no attestation?")
- Per-model token and cost breakdown

## Core Principle

**Attestations are graph-level annotations on a set of changes. Stacks are views.
Attestations transcend views.**

```
THE GRAPH (stack-independent)
══════════════════════════════

  ┌──────────┐     ┌──────────┐     ┌──────────┐
  │ Change A │     │ Change B │     │ Change C │
  │ auth.rs  │     │ login()  │     │ tests    │
  └──────────┘     └──────────┘     └──────────┘
       │                │                │
       └────────────────┴────────────────┘
                        │
                        ▼
                 ┌──────────────┐
                 │ Attest  XMJZ │
                 │ $0.57, 526k  │
                 └──────────────┘

STACKS (views — just changelogs, no attestations)
══════════════════════════════════════════════════

  dev:      [A, B, C, D, E]
  release:  [A, B, C]
  test:     [A, B]
```

## Node Type

```rust
// atomic-core/src/pristine/tables.rs
pub mod node_type {
    pub const CHANGE: u8 = 0;
    pub const TAG: u8 = 1;
    pub const ATTESTATION: u8 = 2;
}
```

Attestations are registered in EXTERNAL/INTERNAL tables (content-addressed by hash)
and in NODE_TYPES with type `2`. They are NOT added to any stack's STACK_CHANGES
table.

## File Format

Stored as `{hash}.attest` in `.atomic/changes/{prefix}/`:

```
[MAGIC: 4 bytes "ATST"]
[VERSION: 1 byte]
[bincode payload → Attestation struct]
```

### Attestation Struct

```rust
// atomic-core/src/change/attestation.rs

/// An attestation is a graph-level audit node that captures metadata
/// about a set of changes — cost, tokens, model usage, duration.
///
/// Attestations have dependencies (the changes they cover) but produce
/// zero hunks — they don't modify the content graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// Format version for forward compatibility.
    pub version: u8,

    /// Timestamp when the attestation was created.
    pub timestamp: DateTime<Utc>,

    /// The agent that produced the covered changes.
    pub agent: AttestAgent,

    /// Session identifier linking turns together.
    pub session_id: String,

    /// Total cost in USD across all models.
    pub cost_usd: f64,

    /// Duration the API spent processing (milliseconds).
    pub duration_api_ms: u64,

    /// Wall clock duration of the session/segment (milliseconds).
    pub duration_wall_ms: u64,

    /// Code change statistics.
    pub code_changes: CodeChangeStats,

    /// Per-model token usage and cost breakdown.
    pub models: Vec<ModelUsage>,

    /// Hashes of the changes this attestation covers.
    ///
    /// These are also registered in the DEPS table so the graph
    /// knows the relationship. This field is denormalized for
    /// fast access without a DEPS lookup.
    pub changes_covered: Vec<Hash>,

    /// Hash of the previous attestation in this session (if any).
    ///
    /// On session resume, a new attestation chains to the previous
    /// one. This forms a linked list of attestations within a session
    /// without requiring a stack.
    pub previous_attestation: Option<Hash>,

    /// Optional free-form notes.
    pub notes: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestAgent {
    /// Agent registry key (e.g., "claude-code").
    pub name: String,
    /// Human-readable name (e.g., "Claude Code").
    pub display_name: String,
    /// AI vendor (e.g., "anthropic").
    pub vendor: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeChangeStats {
    pub lines_added: u64,
    pub lines_removed: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Model identifier (e.g., "claude-sonnet-4-5").
    pub model: String,
    /// Input/prompt tokens.
    pub input_tokens: u64,
    /// Output/completion tokens.
    pub output_tokens: u64,
    /// Cache read tokens.
    pub cache_read_tokens: u64,
    /// Cache write tokens.
    pub cache_write_tokens: u64,
    /// Cost in USD for this model's usage.
    pub cost_usd: f64,
}
```

## Graph Relationships

### Dependencies (DEPS table)

```
Attestation XMJZ → depends on → [Change A, Change B, Change C]
```

The attestation's `changes_covered` hashes are also registered in the DEPS table:
```
DEPS[attest_id] = [change_id_a, change_id_b, change_id_c]
REV_DEPS[change_id_a] = [..., attest_id]
REV_DEPS[change_id_b] = [..., attest_id]
REV_DEPS[change_id_c] = [..., attest_id]
```

This means:
- Given an attestation → find its changes (DEPS lookup)
- Given a change → find its attestations (REV_DEPS lookup, filter by node_type = ATTESTATION)

### Chaining on Session Resume

```
Session starts:
  Change A → Change B → Change C
  Attest₁ deps: [A, B, C]

Session resumes:
  Change D → Change E
  Attest₂ deps: [D, E], previous_attestation: Attest₁

Query "full session cost":
  Follow Attest₂.previous_attestation → Attest₁
  Sum: Attest₁.cost_usd + Attest₂.cost_usd
```

### Cross-Stack Queries

An attestation is NOT in any stack's changelog. To find attestations for a stack:

```
1. Get all change IDs in the stack (STACK_CHANGES)
2. For each change, check REV_DEPS for attestation nodes
3. Filter REV_DEPS results by node_type = ATTESTATION
4. Deduplicate (multiple changes may point to same attestation)
5. For each attestation, resolve which changes are in which stacks
```

This produces:

```
Attest XMJZ — Claude Code · $0.57 · 526k tokens · 2m 52s
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  Changes covered:

  ┌─────────────┬──────────────────────┬─────────────────────┐
  │ Change      │ Message              │ Stacks              │
  ├─────────────┼──────────────────────┼─────────────────────┤
  │ ANU52ZQU    │ Add auth module      │ dev, release, test  │
  │ RIE62WWC    │ Fix token validation │ dev, release        │
  │ K22T3C7E    │ Add tests            │ dev                 │
  └─────────────┴──────────────────────┴─────────────────────┘

  Coverage:
    dev      ███████████ 3/3 changes (100%)
    release  ███████░░░░ 2/3 changes
    test     ███░░░░░░░░ 1/3 changes
```

## Stack Interaction

| Operation | Attestation Behavior |
|-----------|---------------------|
| `stack list` | Not shown (attestations aren't in changelogs) |
| `stack delete` | No effect on attestations (they're in the graph) |
| `stack new --from` | No effect (only copies changelog, attestations are separate) |
| `unrecord` | No effect on attestations |
| `apply from-stack` | Only applies changes; attestations stay in graph |
| `log` | Can optionally show attestations for changes in the log |
| `push` | Attestations push as separate graph nodes |
| `pull` | Attestations pull as separate graph nodes |

## Protocol

### New Operation: `?attest={hash}`

```
POST ?attest={hash}
Body: raw .attest file bytes

Server:
  1. Verify hash matches content
  2. Store as {hash}.attest in .atomic/changes/{prefix}/
  3. Register in EXTERNAL/INTERNAL with node_type::ATTESTATION
  4. Register dependencies in DEPS from changes_covered
  5. Return: { "success": true, "hash": "{hash}" }
```

### Push Flow

```
atomic push:
  1. Upload changes (existing ?apply flow)
  2. Upload attestations (?attest flow)
     - Only attestations whose changes_covered are all on the remote
```

### Pull Flow

```
atomic pull:
  1. Download changes (existing flow)
  2. Download attestations
     - Query: attestations referencing changes we just pulled
```

## API Endpoints

### GET /attestations?stack={name}

Returns attestations for changes in the given stack.

```json
[
  {
    "hash": "XMJZ3IPF...",
    "agent": {
      "name": "claude-code",
      "display_name": "Claude Code",
      "vendor": "anthropic"
    },
    "session_id": "82ba16d0-fd3d-49b9-a65f-92c1911085cd",
    "cost_usd": 0.57,
    "duration_api_ms": 172000,
    "duration_wall_ms": 2354000,
    "code_changes": { "lines_added": 263, "lines_removed": 8 },
    "models": [
      {
        "model": "claude-sonnet-4-5",
        "input_tokens": 176,
        "output_tokens": 8400,
        "cache_read_tokens": 526900,
        "cache_write_tokens": 13100,
        "cost_usd": 0.56
      },
      {
        "model": "claude-haiku-4-5",
        "input_tokens": 9400,
        "output_tokens": 403,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "cost_usd": 0.0115
      }
    ],
    "changes": [
      {
        "hash": "ANU52ZQU...",
        "message": "Add auth module",
        "stacks": ["dev", "release", "test"]
      },
      {
        "hash": "RIE62WWC...",
        "message": "Fix token validation",
        "stacks": ["dev", "release"]
      },
      {
        "hash": "K22T3C7E...",
        "message": "Add tests",
        "stacks": ["dev"]
      }
    ],
    "coverage": {
      "dev": { "covered": 3, "total": 5, "percentage": 60 },
      "release": { "covered": 2, "total": 3, "percentage": 67 },
      "test": { "covered": 1, "total": 2, "percentage": 50 }
    },
    "previous_attestation": null,
    "timestamp": "2026-02-11T14:47:14Z"
  }
]
```

### GET /attestations?change={hash}

Returns attestations covering a specific change.

### GET /attestations/summary?stack={name}

Returns aggregated attestation data for a stack (summed across all
attestations whose changes overlap with the stack).

```json
{
  "stack": "dev",
  "total_cost_usd": 1.23,
  "total_changes_covered": 12,
  "total_changes_in_stack": 15,
  "coverage_percentage": 80,
  "agents": [
    { "name": "claude-code", "display_name": "Claude Code", "changes": 10, "cost_usd": 1.10 },
    { "name": "gemini-cli", "display_name": "Gemini CLI", "changes": 2, "cost_usd": 0.13 }
  ],
  "models": [
    { "model": "claude-sonnet-4-5", "total_tokens": 1200000, "cost_usd": 1.05 },
    { "model": "claude-haiku-4-5", "total_tokens": 50000, "cost_usd": 0.05 },
    { "model": "gemini-2.5-pro", "total_tokens": 80000, "cost_usd": 0.13 }
  ]
}
```

## UI

### Change Header — Attestation Badge

When viewing a change that has an attestation, show a compact badge:

```
┌─────────────────────────────────────────────────────────────┐
│ fix authentication bug in login.rs                          │
│                                                             │
│ leefaus · Feb 11, 2026 at 4:25 PM · RIE62WWC2WXH           │
│                                                             │
│ Depends on  [ANU52ZQUHRCJ Add auth module]                  │
│                                                             │
│ ┌ Attested ─────────────────────────────────────────────┐   │
│ │ Claude Code · $0.57 · 526k tokens · 2m 52s API       │   │
│ │ sonnet-4-5: 527k tok  ·  haiku-4-5: 9.8k tok        │   │
│ │ Covers 3 changes across dev, release, test            │   │
│ └───────────────────────────────────────────────────────┘   │
│                                                             │
│ 1 file changed  +12 -3                                      │
└─────────────────────────────────────────────────────────────┘
```

### Stack Dropdown — Attestation Summary

Show aggregated cost/coverage in the stack dropdown:

```
┌─────────────────────────────────────────────┐
│ ◇ dev                                    5  │
│   Claude Code · $1.23 · 80% attested        │
├─────────────────────────────────────────────┤
│ ◇ release                                3  │
│   Claude Code · $0.57 · 67% attested        │
├─────────────────────────────────────────────┤
│ ◇ test                                   2  │
│   Claude Code · $0.12 · 50% attested        │
└─────────────────────────────────────────────┘
```

### Attestation Detail View

Full attestation view accessible by clicking the badge or from a
dedicated attestation list:

```
Attest XMJZ — Claude Code · $0.57 · 526k tokens · 2m 52s
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  Model Breakdown:
    claude-sonnet-4-5   526.9k cache read · 8.4k output · $0.56
    claude-haiku-4-5    9.4k input · 403 output · $0.01

  Changes covered:

  ┌─────────────┬──────────────────────┬─────────────────────┐
  │ Change      │ Message              │ Stacks              │
  ├─────────────┼──────────────────────┼─────────────────────┤
  │ ANU52ZQU    │ Add auth module      │ dev, release, test  │
  │ RIE62WWC    │ Fix token validation │ dev, release        │
  │ K22T3C7E    │ Add tests            │ dev                 │
  └─────────────┴──────────────────────┴─────────────────────┘

  Coverage:
    dev      ███████████ 3/3 changes (100%)
    release  ███████░░░░ 2/3 changes
    test     ███░░░░░░░░ 1/3 changes

  Code:  +263 lines added, -8 lines removed
  Wall:  39m 14s  ·  API: 2m 52s
```

## Creation Flow

### Agent Hook (automatic)

When a Claude Code session ends (or on resume):

```
1. Parse `claude --resume {session_id}` output
2. Extract: cost, tokens by model, duration, code stats
3. Collect hashes of changes recorded during this session
4. Build Attestation struct
5. Serialize to .attest file
6. Hash and store in .atomic/changes/
7. Register in graph: EXTERNAL, INTERNAL, NODE_TYPES, DEPS
8. Push with next `atomic push`
```

### Manual (CLI)

```bash
# Create attestation from session data
atomic attest --session 82ba16d0 --cost 0.57 --duration-api 2m52s

# Or pipe from claude CLI
claude --resume 82ba16d0 --json | atomic attest --from-claude-summary
```

## Implementation Phases

### Phase 1: Core Types
- [ ] `Attestation` struct in `atomic-core/src/change/attestation.rs`
- [ ] `node_type::ATTESTATION = 2` constant
- [ ] Serialize/deserialize with magic prefix + bincode
- [ ] `register_attestation()` in MutTxnT
- [ ] DEPS registration for attestation → changes

### Phase 2: Storage & Retrieval
- [ ] Store `.attest` files alongside `.change` files
- [ ] Load attestation by hash
- [ ] Query: find attestations for a change (REV_DEPS + node_type filter)
- [ ] Query: find attestations for a stack (iterate stack changes, REV_DEPS)

### Phase 3: Protocol
- [ ] `?attest={hash}` POST operation in atomic-api
- [ ] `upload_attestation()` in atomic-remote HttpRemote
- [ ] Push: upload attestations after changes
- [ ] Pull: download attestations for received changes

### Phase 4: CLI
- [ ] `atomic attest` command for manual creation
- [ ] `atomic attest --from-claude-summary` for piping session data
- [ ] `atomic log --attestations` to show attestations inline
- [ ] `atomic push` includes attestations

### Phase 5: Agent Integration
- [ ] Parse `claude --resume` output in atomic-agent
- [ ] Create attestation at session end in turn orchestrator
- [ ] Chain attestations on session resume (previous_attestation)

### Phase 6: API
- [ ] `GET /attestations?stack={name}` endpoint
- [ ] `GET /attestations?change={hash}` endpoint
- [ ] `GET /attestations/summary?stack={name}` endpoint
- [ ] Include attestation badge data in ChangeInfo response

### Phase 7: UI
- [ ] Attestation badge in change header
- [ ] Attestation summary in stack dropdown
- [ ] Attestation detail view (model breakdown, coverage, changes)
- [ ] Coverage percentage indicators