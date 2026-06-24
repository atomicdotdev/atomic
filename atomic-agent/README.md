# atomic-agent

AI agent integration for [Atomic VCS](https://github.com/atomicdotdev/atomic) — turn-level recording of AI coding sessions with full provenance.

Every agent turn becomes a single Atomic change. Session data lives inside the change itself — it commutes via patch theory, pushes to remotes automatically, and renders in server UI without a separate sync protocol.

## What It Does

```
You prompt Claude Code → agent modifies files → Atomic records the turn
                                                  │
                                                  ├── ChangeHeader: "Turn 3: Fix the auth bug"
                                                  ├── Provenance: anthropic/claude-sonnet-4, tokens, cost
                                                  ├── SessionEnvelope: turn #3, timing, files (hashed)
                                                  └── Transcript: conversation (unhashed, redactable)
```

Each turn is a proper content-addressed Atomic change — diffable, revertable, blameable at the token level.

## Quick Start

```bash
# Initialize an Atomic repository
atomic init

# Install agent hooks (Claude Code, Gemini CLI, Codex, OpenCode)
atomic agent enable --agent claude-code

# Work normally with your AI agent — turns are recorded automatically

# See what was recorded
atomic log

# Check session status
atomic agent status --verbose

# Generate reasoning summary for a session
atomic agent explain <session-id>

# Save reasoning to change + learnings to CLAUDE.md
atomic agent explain <session-id> --save

# Rewind to a previous turn
atomic unrecord

# Disable
atomic agent disable
```

## How It Works

When you run `atomic agent enable`, hooks are installed in the agent's configuration file (e.g., `.claude/settings.json`). These hooks call back to `atomic agent hooks <agent> <verb>` at lifecycle points:

| Agent Event | What Happens |
|---|---|
| **Session Start** | Session created, tracking message returned to agent |
| **User Prompt Submit** | Turn timer started, prompt captured |
| **Agent Stop** | `status → add untracked → record all` — change created with provenance |
| **Session End** | Session marked as ended |
| **Tool Use** | Tool invocations logged (sub-turn recording planned) |

The recording workflow on each turn end:

1. **Status** — ask the repository what changed since the last recorded state
2. **Add** — track any new files the agent created (filtering out `node_modules`, `target`, etc.)
3. **Record** — create an Atomic change with AI provenance metadata

No daemon. No Watchman required. No git. Each hook invocation is a standalone process that opens the repo, does its work, and exits.

## Agent Identity

Agent changes are attributed using a `+tag` email format derived from the user's default identity in `~/.atomic/identities/`. This ties every agent change to the human who authorized it while making it immediately clear in logs, blame, and UI that the change came from an agent.

```
User identity:   Lee Faus <lee@atomic.dev>
                      │         │
Agent author:    claude+60f5 <lee@atomic.dev>
                  │      │         │
                  │      │         └── User's email (unchanged — replies reach the human)
                  │      └── First 4 hex chars of session ID (disambiguates concurrent sessions)
                  └── Agent name (first segment before hyphen)
```

The format varies by agent and session:

| Agent | Session ID | Author |
|---|---|---|
| Claude Code | `60f5cbd2-aa23-...` | `claude+60f5 <lee@atomic.dev>` |
| Gemini CLI | `abcd1234-...` | `gemini+abcd <lee@atomic.dev>` |
| Codex | `9876fedc-...` | `codex+9876 <lee@atomic.dev>` |

**Resolution order:**

1. Look up the user's default identity in `~/.atomic/identities/config.toml`
2. Load the identity's name, email, and public key from `identity.toml`
3. Construct `{agent}+{session_short} <{user_email}>` with the user's public key reference
4. If no identity is configured, fall back to `{Agent Display Name}` with no email

The public key reference in the `Author.identity` field means the change is cryptographically linked to the user's Ed25519 keypair — even though the agent created it.

## Session Data in the Change

Atomic changes have three data slots. Session data maps onto them:

| Slot | Data | Hashed? | Purpose |
|---|---|---|---|
| `hashed.provenance` | Vendor, model, tokens, cost, prompt hash, session ID | **Yes** | Tamper-evident per-turn attribution |
| `hashed.metadata` | SessionEnvelope (turn #, timing, files, agent name) | **Yes** | ✅ Tamper-evident session structure |
| `unhashed` | Transcript (full conversation, tool use) | **No** | Large/redactable, travels with change |

Because provenance and the session envelope are hashed, they're part of the change's cryptographic identity. Forging session metadata would change the hash. The transcript is unhashed so it can be stripped from public repositories without invalidating any changes.

When you `atomic push`, the server receives changes with session data already inside them. No separate metadata branch, no checkpoint IDs, no side-channel sync.

## Explain: Reasoning + Knowledge Flywheel

The `explain` command generates AI reasoning summaries for recorded turns by calling Claude CLI on the condensed transcript.

```bash
# Explain the most recent turn
atomic agent explain <session-id>

# Explain a specific turn
atomic agent explain <session-id> --turn 3

# Explain all turns and save everything
atomic agent explain <session-id> --all --save
```

**What `explain` produces:**

```
Turn 1 — Turn 1: Fix the auth bug in login.rs
  ├── Intent: Fix authentication bug preventing user login
  ├── Outcome: Fixed token validation, all tests passing
  ├── Learnings:
  │   ├── src/auth/login.rs:42 (validate_token) — Token expiry compared with wrong timezone [bug]
  │   ├── Repo: Auth module uses JWT with RS256, not HS256
  │   └── Workflow: cargo test --lib is faster for auth changes
  ├── Friction: 3 layers of middleware to find the validation
  └── Open: Refresh endpoint has the same bug (deferred)
```

Code learnings (with file:line references) are anchored to the CRDT graph so they follow the code through renames and refactors. Users see files, functions, and lines — the graph anchoring is invisible.

**What `--save` does:**

1. **Saves reasoning into the change's unhashed section** — travels on push, rendered by server UI
2. **Appends Repo + Workflow learnings to the agent's context file** — the knowledge flywheel

Only **Repo** and **Workflow** learnings are written to the context file. Code learnings (with file:line references) stay in the change's unhashed section — they're noisy in a context file, they drift with edits, and in a large project they'd grow unbounded.

| Agent | Context File | What Gets Written |
|---|---|---|
| Claude Code | `CLAUDE.md` | Repo patterns + Workflow tips |
| Gemini CLI | `GEMINI.md` | Repo patterns + Workflow tips |
| Codex | `codex.md` | Repo patterns + Workflow tips |
| Unknown | `.atomic/learnings.md` | Repo patterns + Workflow tips |

The context file uses fenced markers so the module can merge new learnings into existing headings without duplicating sections:

```markdown
## Agent Learnings

<!-- atomic:learnings:start -->

### Repo
- Auth module uses JWT with RS256, not HS256

### Workflow
- cargo test --lib is faster for auth changes

<!-- atomic:learnings:end -->
```

**The knowledge flywheel:**

```
Agent works → explain --save → learnings in CLAUDE.md
    ↑                                    │
    └── next session reads CLAUDE.md ←───┘
        agent starts smarter
```

Each session accumulates knowledge from previous sessions. Learnings are deduplicated by finding text — running `explain --save` multiple times on the same session won't produce duplicate entries.

## What Gets Pushed (and What Doesn't)

Session data is reconstructed from the change files — not from a separate sync channel. The `.atomic/sessions/*.json` files are **local runtime state** that never leaves the machine.

```
PUSHED (inside each .atomic/changes/ file)        NOT PUSHED (local only)
─────────────────────────────────────────          ──────────────────────────
hashed.provenance:                                 .atomic/sessions/*.json
  vendor, model, session_id, turn_number,            session phase (Idle/Active/Ended)
  tokens, cost, prompt hash, agent_name              turn timer state
                                                     watcher snapshots
hashed.header:
  message: "Turn 3: Fix the auth bug"
  author:  claude+60f5 <lee@atomic.dev>

hashed.hunks:
  the actual file diffs

hashed.metadata:                                   Once the turn is recorded as a
  SessionEnvelope (turn#, timing, files, agent)    change, the session file's job
                                                   is done. It's the orchestrator's
unhashed:                                          scratch pad, not the record.
  condensed transcript, tool summaries,
  reasoning (via explain --save)

                                                   CLAUDE.md / GEMINI.md:
                                                     repo + workflow learnings
                                                     (via explain --save)
                                                     read by agent on next session
```

**The server reconstructs session timelines from change data:**

1. Scan `hashed.provenance[0].session_id` → group changes by session
2. Read `provenance.metadata["turn_number"]` → order turns within session
3. Read `header.authors[0]` → `claude+60f5 <lee@atomic.dev>` identifies agent + human
4. Read `provenance.vendor` / `provenance.model` → which AI, which model
5. Aggregate `provenance.tokens` / `provenance.cost` → usage dashboards

No separate session API. No metadata branch. One `push` sends everything. One `pull` fetches everything. The server reads it from the changes it already has.

## Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│  Agent Hook (stdin JSON)                                                 │
│       │                                                                  │
│       ▼                                                                  │
│  hooks::AgentHook::parse_event()          ← Claude Code, Gemini CLI,    │
│       │                                      Codex, OpenCode adapters    │
│       ▼                                                                  │
│  event::TurnEvent                                                        │
│       │                                                                  │
│       ▼                                                                  │
│  turn::TurnOrchestrator::dispatch()                                      │
│       │                                                                  │
│       ├── turn::phase::transition()       ← State machine (pure fn)      │
│       │   Idle → Active → Idle → Ended                                   │
│       │                                                                  │
│       ├── turn::SessionStore              ← .atomic/sessions/*.json      │
│       │                                                                  │
│       └── record::record_turn()           ← status → add → record        │
│               │                                                          │
│               ├── ChangeHeader             "Turn 3: Fix the auth bug"    │
│               ├── Provenance               anthropic, claude-sonnet-4    │
│               ├── SessionEnvelope          turn context (bincode+magic)  │
│               └── repo.record(all: true)   repository does the diff     │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

## Modules

| Module | Purpose |
|---|---|
| `error` | `AgentError` enum — Watchman, hook parse, session, turn, recording errors with classification and suggestions |
| `event` | `HookType`, `TurnEvent`, `TurnChanges` — normalized lifecycle events from any agent |
| `hooks` | `AgentHook` trait, `AgentRegistry`, Claude Code adapter (Gemini/Codex/OpenCode planned) |
| `hooks::claude_code` | Parse Claude Code JSON, install/uninstall hooks in `.claude/settings.json` |
| `watcher` | `FileWatcher` trait, `FallbackWatcher` (walkdir snapshots), Watchman backend (planned) |
| `turn::phase` | Phase/Event/Action state machine — `transition()` pure function, 20 transitions |
| `turn::session` | `AgentSession` + `SessionStore` — JSON persistence in `.atomic/sessions/` |
| `turn::orchestrator` | `TurnOrchestrator` — dispatches events through state machine → recording |
| `envelope` | `SessionEnvelope` — bincode-encoded turn metadata for `HashedChange.metadata` |
| `record` | `record_turn()` — status → add → record workflow with Provenance + SessionEnvelope |
| `identity` | Agent author resolution — derives `+tag` email from user's default identity |
| `transcript` | Condensed transcript parsing, reasoning types, graph anchoring, unhashed attach/extract/strip |
| `learnings` | Write Repo + Workflow learnings to agent context files (CLAUDE.md, GEMINI.md, etc.) |

## How this differs from Entire CLI

This crate implements [Entire CLI](https://github.com/entireio/cli) (~12,000 lines of Go) capabilities. Atomic goes futhur by integrating natively into the atomic-cli and leverages the navtive stack and GraphOps capabilities with identity mapping and provenance (~12,000 lines of Rust including tests):

| Entire CLI | atomic-agent | Why |
|---|---|---|
| Shadow git branches (`entire/<hash>`) | Atomic stacks (`agent/{session_id}`) | Stacks are native to Atomic |
| Orphan metadata branch (`entire/checkpoints/v1`) | Change `metadata` + `unhashed` fields | Session data is in the change |
| 12-hex checkpoint IDs + commit trailers | Merkle state hashing | Content-addressed by design |
| Git status diffing (snapshot twice, diff) | `repo.status()` + `repo.record(all: true)` | Repository knows what changed |
| `entire rewind` (restore from shadow branch) | `atomic unrecord` | Already implemented |
| `entire enable/disable/status` | `atomic agent enable/disable/status` | Same UX, native VCS |
| Go + git + shadow branches | Rust + redb + patch theory | No parallel VCS inside a VCS |

## CLI Commands

```
atomic agent enable [--agent NAME] [--force] [--all]
atomic agent disable [--agent NAME] [--all]
atomic agent status [--verbose]
atomic agent explain <session-id> [--turn N] [--all] [--save] [--model MODEL]
atomic agent hooks <agent> <verb>              # internal, called by agent hooks
```

## Supported Agents

| Agent | Status | Hook Format |
|---|---|---|
| **Claude Code** | ✅ Implemented | `.claude/settings.json`, 7 hooks |
| **Cline** | ✅ Implemented | `.cline/settings.json` |
| **Codex** | ✅ Implemented | `.codex/hooks.json` |
| **Copilot** | ✅ Implemented | `.github/copilot-hooks.yml` |
| **Cursor** | ✅ Implemented | `.cursor/hooks.json` |
| **Gemini CLI** | ✅ Implemented | `.gemini/settings.json` |
| **Kiro** | ✅ Implemented | IDE panel + shell scripts (via `atomic-kiro` package) |
| **OpenCode** | ✅ Implemented | `.opencode/hooks.json` |
| **Pi** | ✅ Implemented | Extension-based (`atomic-pi` package) |
| **Sherpa** | ✅ Implemented | Self-managed by TUI |

Adding a new agent requires implementing the `AgentHook` trait (~150 lines) and registering it in `AgentRegistry::with_defaults()`.

## Storage Layout

```
.atomic/
├── pristine/              # Graph database (Atomic core)
├── changes/               # Content-addressed change files
├── sessions/              # Agent session state (NEW)
│   ├── 2026-01-15-abc123.json
│   └── 2026-01-15-def456.json
├── config.toml
└── working_copy_id
```

Session state files are lightweight JSON (~500 bytes each). Turn data is in the Atomic changes themselves — not in separate files.

## Tests

612 tests (584 unit + 28 doc-tests), 0 failures.

```
error.rs          22 tests    Error types, classification, suggestions, exit codes
event.rs          47 tests    HookType, TurnEvent, TurnChanges
hooks/mod.rs      27 tests    AgentHook trait, AgentRegistry
hooks/claude_code 52 tests    JSON parsing, install/uninstall, filesystem roundtrips
watcher/fallback  33 tests    Snapshot diffing, ignore patterns, lifecycle
watcher/mod.rs    10 tests    FileWatcher trait, WatcherConfig
turn/phase.rs     56 tests    All 20 state transitions × 2 contexts, apply_common_actions
turn/session.rs   49 tests    AgentSession, SessionStore CRUD, validation, lifecycle
turn/orchestrator 19 tests    Full dispatch lifecycle, Ctrl-C recovery
envelope.rs       47 tests    Encode/decode, schema versioning, edge cases
record.rs         51 tests    Header/provenance/envelope construction, ignore patterns, unhashed attach
transcript.rs     78 tests    Condensed parsing, reasoning types, graph anchoring, attach/extract/strip, Claude CLI generator
identity.rs       35 tests    +tag derivation, session short, TOML parsing, resolve with/without identity
learnings.rs      68 tests    Context file resolution, markdown format, dedup, merge headings, save/load
lib.rs             9 tests    Integration, cross-module usage
```

## Known Limitations

1. **Sub-agent sub-turn recording not implemented** — PreToolUse/PostToolUse events are logged but don't create separate changes for sub-agent work. Fix: add `begin_sub_turn()`/`end_sub_turn()` to orchestrator.

2. **Watchman backend not wired up** — `create_watcher()` returns the fallback (walkdir) watcher. Watchman would give O(changed-files) detection instead of relying on `repo.status()`. Fix: implement `WatchmanTurnWatcher` with clock + since queries.

3. **Reasoning dedup is exact-match** — LLMs generate different wording for the same concept across runs, so `explain --save` on the same session twice may produce near-duplicate learnings in the context file. Fix: embedding-based similarity dedup (future).

See [ATOMIC-AGENT-TASKS.md](../ATOMIC-AGENT-TASKS.md) for the full task list with phase tracking.

## License

Dual-licensed under MIT and Apache 2.0.
