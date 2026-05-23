# Atomic Agent: Task List

> **Goal**: Absorb Entire CLI's capabilities directly into Atomic using Watchman as the file-watching engine. Turns become changes, sessions become stacks, rewind becomes unrecord, metadata lives inside the change itself. No git involvement. No side branches. No parallel VCS.

## What Entire CLI Does That Atomic Needs to Absorb

| Entire Capability | Entire Implementation | Atomic Replacement |
|---|---|---|
| Install agent hooks | `agent/claudecode/hooks.go`, `agent/geminicli/hooks.go` | `atomic-agent/src/hooks/` |
| Turn lifecycle state machine | `session/phase.go` (IDLE→ACTIVE→ACTIVE_COMMITTED→ENDED) | `atomic-agent/src/turn/phase.rs` |
| File change detection | `state.go` — snapshots git status twice, diffs | Watchman `since`-clock queries |
| Transcript capture | JSONL/JSON parsing, prompt extraction | Change `metadata` field + zstd compression |
| Checkpoint storage | Orphan git branch `entire/checkpoints/v1` | **Deleted.** Changes ARE checkpoints |
| Shadow branches | `entire/<hash>-<worktree>` branches | **Deleted.** Agent stacks: `agent/{session_id}` |
| Checkpoint IDs + trailers | 12-hex-char IDs, `Entire-Checkpoint` trailer | **Deleted.** Merkle state hashing |
| Rewind to checkpoint | Restore files from shadow branch tree | `atomic unrecord` (already exists) |
| Agent attribution | None (just commit author) | `Provenance` + `CreditType` + delegated identity (already exist) |

## What Atomic Already Has

- [x] **AI Provenance** — `Provenance` struct: vendor, model, tool, tokens, cost, suggestion type (`atomic-core/src/change/provenance.rs`)
- [x] **Credit/Blame** — `CreditType` enum: Human, AiAssisted, AiGenerated (`atomic-core/src/change/credit.rs`)
- [x] **Identity Delegation** — User/Agent/Delegated types, `DelegationPermission`, delegation certificates (`atomic-identity/`)
- [x] **Change Recording** — Full record workflow with hunks, CRDT semantic layer, diff algorithms (`atomic-repository/src/record.rs`)
- [x] **Stacks** — Views of the graph, not divergent histories (`atomic-core/src/pristine/`)
- [x] **Unrecord** — Remove change from stack, preserves change data (`atomic-repository/src/unrecord.rs`)
- [x] **Content-Addressed Storage** — Changes are immutable, content-addressed, composable

## Estimated Reduction

| Entire CLI Component | Lines of Go | Replaced By |
|---|---|---|
| `agent/` (hook parsing, 6 files) | ~1,200 | `atomic-agent/src/hooks/` (~400 lines Rust) |
| `strategy/` (manual-commit, auto-commit, 20 files) | ~4,000 | **Deleted.** Atomic stacks + record IS the strategy |
| `checkpoint/` (git tree building, shadow branches) | ~800 | **Deleted.** Changes ARE checkpoints |
| `session/` (state files in `.git/entire-sessions/`) | ~600 | `atomic-agent/src/turn/session.rs` (~200 lines) |
| `state.go` (file change detection via git status) | ~600 | `atomic-agent/src/watcher/` (~150 lines) — Watchman |
| `hooks_claudecode_handlers.go` (turn lifecycle) | ~870 | `atomic-agent/src/turn/orchestrator.rs` (~200 lines) |
| `hooks_geminicli_handlers.go` | ~650 | Same hooks/ adapter (~100 lines) |
| Rewind (restore files from shadow branch trees) | ~500 | `atomic unrecord` — already exists |
| Metadata branch (`entire/checkpoints/v1`) | ~800 | **Deleted.** Metadata lives in `HashedChange.metadata` |
| Settings, config, doctor, clean, resume, etc. | ~2,000 | `atomic agent enable/disable/status` (~300 lines) |
| **Total** | **~12,000** | **~1,350 lines of Rust** |

## Key Architecture: Session Data Lives Inside Changes

Session/turn data is embedded in the change itself using three existing slots in
Atomic's change format. This means session data **commutes** via patch theory,
**pushes** to remotes automatically, and is **available on the server** for UI
rendering — no separate sync, no metadata branch, no side-channel.

| Data | Slot | Hashed? | Purpose |
|---|---|---|---|
| Turn metrics (tokens, cost, model, prompt hash) | `hashed.provenance` | **Yes** | Tamper-evident per-turn attribution |
| Session envelope (turn #, session ID, timing, files) | `hashed.metadata` | **Yes** | Tamper-evident session structure |
| Transcript (full conversation, prompts, tool use) | `unhashed` | **No** | Large/redactable, travels with change |

See **Phase 18.5** for full implementation details.

---

## Phase 14: `atomic-agent` Crate Foundation ✅

### 14.1 Crate Scaffold ✅
- [x] Create `atomic-agent/Cargo.toml` with dependencies:
  - `watchman_client = "0.9"` (Watchman Rust client)
  - `atomic-core` (workspace)
  - `atomic-repository` (workspace)
  - `atomic-identity` (workspace)
  - `serde`, `serde_json`, `tokio`, `chrono`, `thiserror`, `anyhow`, `zstd`
- [x] Add `atomic-agent` to workspace `members` in `atomic/Cargo.toml`
- [x] Create `atomic-agent/src/lib.rs` with module declarations
- [x] Create `atomic-agent/src/error.rs` — `AgentError` enum:
  - `WatchmanNotRunning`, `WatchmanConnectionFailed`, `WatchmanQueryFailed`, `WatchmanResolveRoot`
  - `HookParseFailed`, `HookInputEmpty`, `HookFieldMissing`
  - `SessionNotFound`, `SessionSaveFailed`, `SessionLoadFailed`, `SessionIdInvalid`, `SessionConflict`
  - `TurnNotActive`, `TurnAlreadyActive`, `SessionEnded`
  - `RecordFailed`, `EmptyTurn`, `StackError`
  - `ConfigError`, `NotARepository`, `AgentNotFound`, `AlreadyInstalled`
  - `IdentityError`, `DelegationError`, `TranscriptReadFailed`, `EnvelopeCodecError`
  - Classification methods: `is_recoverable()`, `is_watchman_unavailable()`, `is_state_violation()`
  - `suggestion()` for user-facing hints, `exit_code()` for CLI
- [x] Unit tests: 30 tests (error display, classification, suggestions, exit codes, conversions, Send+Sync)

### 14.2 Turn Event Types ✅
- [x] Create `atomic-agent/src/event.rs`
  - [x] `HookType` enum: `SessionStart`, `SessionEnd`, `TurnStart`, `TurnEnd`, `PreToolUse`, `PostToolUse`
    - `is_turn_boundary()`, `is_session_boundary()`, `is_tool_use()`
    - `from_verb()` — normalizes agent-specific verbs (Claude Code + Gemini CLI)
    - Serde roundtrip, Display
  - [x] `TurnEvent` struct with builder pattern:
    - `session_id`, `event_type`, `transcript_path`, `prompt`, `tool_name`, `tool_use_id`, `timestamp`, `raw_json`
    - `with_*()` builder methods, `has_prompt()`, `prompt_summary(max_len)`, `is_sub_agent()`
    - Display, Serde roundtrip with `skip_serializing_if`
  - [x] `TurnChanges` struct with builder pattern:
    - `modified`, `added`, `deleted`, `timestamp`
    - `is_empty()`, `file_count()`, `all_paths()`, `all_path_strings()`
    - `merge()` with dedup, `summary()` for log messages
    - Display, Serde roundtrip
  - [x] Unit tests: 75 tests (HookType verbs, TurnEvent builder/display/serde, TurnChanges merge/summary/paths)

### 14.3 Agent Hook Trait ✅
- [x] Create `atomic-agent/src/hooks/mod.rs`
  - [x] `AgentHook` trait (dyn-compatible, Send+Sync+Debug):
    - `name()`, `display_name()` — registry key and human name
    - `parse_event(hook_type, input) -> AgentResult<TurnEvent>`
    - `install(repo_root) -> AgentResult<usize>`, `uninstall(repo_root) -> AgentResult<()>`
    - `is_installed(repo_root) -> bool`, `supported_hooks() -> Vec<HookType>`
    - `detect_presence(repo_root) -> bool` (default: false)
    - `hook_verbs() -> Vec<&str>` — agent-specific CLI verb names
  - [x] `AgentRegistry` struct:
    - `new()`, `with_defaults()` (auto-registers Claude Code)
    - `register()`, `get()`, `require()` (returns error with available list)
    - `list()` (sorted), `count()`, `is_empty()`
    - `detect(repo_root)`, `installed(repo_root)`, `iter()`
  - [x] Unit tests: 40 tests (MockAgent, registry CRUD, detect/installed filtering, trait object safety, Send+Sync)

### 14.4 Discovery Module Foundation ✅
_Issue #13. Read-path counterpart to hooks — adapters scan agent storage already on disk and feed the provenance import pipeline._

- [x] Create `atomic-agent/src/discovery/mod.rs`
  - [x] `TraceDiscovery` trait (dyn-compatible, `Send + Sync + Debug`): `agent_id()`, `display_name()`, `is_available()`, `list_traces()`, `read_events()`, `storage_kind()`
  - [x] `DiscoveryRegistry` struct: `new()`, `with_defaults()` (currently empty — adapters land in #18–#28), `register()`, `get()`, `require()` (returns `AgentError::AdapterNotFound`), `list()`, `available()`, `count()`, `is_empty()`, `iter()`, `Default`, `Debug`
- [x] Create `atomic-agent/src/discovery/types.rs`
  - [x] `DiscoveredTrace` struct: `trace_id`, `agent_id`, `title`, `preview`, `timestamp`, `directory`, `source_path`
  - [x] `DiscoveredEvent` struct: `event_type`, `role`, `text`, `tool_name`, `tool_call_id`, `model_id`, `timestamp`, `order`, `raw_json`
  - [x] `DiscoveredEventType` enum: `UserMessage`, `AssistantText`, `AssistantThinking`, `ToolCall`, `ToolResult`, `Error`
  - [x] `StorageKind` enum: `Jsonl`, `Json`, `Sqlite`
- [x] Unit tests: 20 tests total (13 in `mod.rs` covering registry CRUD, ordering, replace semantics, `available()` filter, `Debug` format, trait object-safety, `Send + Sync`; 7 in `types.rs` covering serde round-trip and rename_all formats)
- [ ] Concrete adapters — deferred to follow-up issues:
  - [ ] Claude Code JSONL adapter (#18)
  - [ ] Gemini CLI JSON adapter (#19)
  - [ ] Codex adapter (#20)
  - [ ] Reader helpers per issue #14

---

## Phase 15: Agent Hook Adapters

Each adapter replaces one agent package from Entire CLI. Same JSON input format from the agents, different output type (`TurnEvent` instead of Go structs).

### 15.1 Claude Code Adapter ✅
_Replaces: `cli/cmd/entire/cli/agent/claudecode/` (~800 lines Go → ~350 lines Rust)_

- [x] Create `atomic-agent/src/hooks/claude_code.rs`
- [x] `ClaudeCodeHook` struct implementing `AgentHook`
- [x] JSON input parsing for each hook type:
  - `SessionStart`/`SessionEnd`/`TurnEnd`: `SessionInfoInput { session_id, transcript_path }`
  - `TurnStart`: `UserPromptInput { session_id, transcript_path, prompt }`
  - `PreToolUse`: `PreToolInput { session_id, transcript_path, tool_use_id, tool_input }`
  - `PostToolUse`: `PostToolInput { session_id, transcript_path, tool_use_id, tool_name, tool_input, tool_response }`
  - Missing/empty `session_id` defaults to "unknown"
  - Raw JSON preserved in `TurnEvent.raw_json` for debugging
- [x] `install()`:
  - Reads/creates `.claude/settings.json` preserving unknown fields
  - Installs 7 hooks: `session-start`, `session-end`, `stop`, `user-prompt-submit`, `pre-task`, `post-task`, `post-todo`
  - Adds `permissions.deny` rule for `Read(./.atomic/metadata/**)`
  - Preserves existing non-Atomic hooks, idempotent (returns 0 on re-install)
- [x] `uninstall()`:
  - Removes hooks with `atomic agent hooks claude-code` prefix
  - Removes permissions deny rule, cleans up empty objects
  - Preserves non-Atomic hooks, no-op if no settings file exists
- [x] `is_installed()` — checks for `atomic agent hooks claude-code` prefix in JSON string
- [x] `detect_presence()` — checks for `.claude/` directory
- [x] `hook_verbs()` — returns all 7 verbs
- [x] Registered in `AgentRegistry::with_defaults()`
- [x] Unit tests: 55 tests (parse all formats, empty/missing fields, install/uninstall filesystem roundtrip, preserve non-Atomic hooks, detect presence, full install→installed→uninstall→!installed roundtrip)

### 15.2 Gemini CLI Adapter
_Replaces: `cli/cmd/entire/cli/agent/geminicli/` (~600 lines Go → ~150 lines Rust)_

- [ ] Create `atomic-agent/src/hooks/gemini_cli.rs`
- [ ] `GeminiCliHook` struct implementing `AgentHook`
- [ ] JSON input parsing (Gemini's message-based format):
  - Session events
  - Tool events: `before_tool`, `after_tool`, `before_agent`, `after_agent`, `before_model`, `after_model`
- [ ] `install()` — write hooks to `.gemini/settings.json`
- [ ] `uninstall()` / `is_installed()`
- [ ] Register in `AgentRegistry`
- [ ] Unit tests

### 15.3 Codex Adapter
_New — Entire CLI doesn't have this_

- [ ] Create `atomic-agent/src/hooks/codex.rs`
- [ ] `CodexHook` struct implementing `AgentHook`
- [ ] Research OpenAI Codex CLI hook format
- [ ] Implement `parse_event()` for Codex's hook JSON
- [ ] `install()` — write hooks to Codex config location
- [ ] `uninstall()` / `is_installed()`
- [ ] Unit tests

### 15.4 OpenCode Adapter
_New — Entire CLI doesn't have this_

- [ ] Create `atomic-agent/src/hooks/opencode.rs`
- [ ] `OpenCodeHook` struct implementing `AgentHook`
- [ ] Research OpenCode hook format
- [ ] Implement `parse_event()` for OpenCode's hook JSON
- [ ] `install()` — write hooks to OpenCode config location
- [ ] `uninstall()` / `is_installed()`
- [ ] Unit tests

---

## Phase 16: Watchman Integration

Replaces Entire's `state.go` functions (`ComputeFileChanges`, `ComputeNewFiles`, `ComputeDeletedFiles`) which snapshot git status twice and diff. Watchman gives exact changes via `since`-clock queries with zero tree scanning.

### 16.1 `FileWatcher` Trait ✅
- [x] Create `atomic-agent/src/watcher/mod.rs`
  - [x] `FileWatcher` trait (dyn-compatible via `Pin<Box<dyn Future>>` returns):
    - `fn begin_turn(&mut self, session_id: &str) -> Pin<Box<dyn Future<Output = AgentResult<()>> + Send + '_>>`
    - `fn end_turn(&mut self) -> Pin<Box<dyn Future<Output = AgentResult<TurnChanges>> + Send + '_>>`
    - `fn cancel_turn(&mut self) -> Pin<Box<dyn Future<Output = AgentResult<()>> + Send + '_>>`
    - `fn is_active(&self) -> bool`
  - [x] `WatcherConfig` struct: `repo_root`, `ignore_patterns` (default: `.atomic`)
    - `new()`, `with_ignore_pattern()` builder, `repo_root()`, `ignore_patterns()`
  - [x] `create_watcher(config) -> AgentResult<Box<dyn FileWatcher>>` — returns `FallbackWatcher` (Watchman backend not yet wired)
  - [x] Comprehensive module docs: architecture diagram, Watchman integration details, fallback behavior, subscription overview
  - [x] Unit tests: 13 tests (WatcherConfig builder/clone/debug, trait object safety, create_watcher returns fallback)

### 16.2 Watchman Connection Manager
- [ ] Create `atomic-agent/src/watcher/watchman_watcher.rs`
  - [ ] `WatchmanConnection` struct:
    - `client: watchman_client::Client`
    - `root: watchman_client::ResolvedRoot`
  - [ ] `WatchmanConnection::connect(repo_root: &Path) -> Result<Self>`:
    1. `Connector::new()` → `connector.connect().await`
    2. `CanonicalPath::canonicalize(repo_root)`
    3. `client.resolve_root(canonical).await`
  - [ ] `WatchmanConnection::is_available() -> bool` — static check if Watchman daemon is reachable
  - [ ] Error handling: `WatcherError::NotRunning`, `WatcherError::ConnectionFailed`, `WatcherError::QueryFailed`
  - [ ] Unit tests: connection creation (skip-if-no-watchman gate)

### 16.3 Watchman Turn Watcher
_Uses `state_enter`/`state_leave` + `clock` + `query(since:)` from `watchman_client`_

- [ ] Create `WatchmanTurnWatcher` struct in `watchman_watcher.rs`:
  - `conn: WatchmanConnection`
  - `turn_start_clock: Option<ClockSpec>`
  - `ignore_expr: Expr` — pre-built expression to exclude `.atomic/`
- [ ] Implement `FileWatcher` for `WatchmanTurnWatcher`:
  - [ ] `begin_turn(session_id)`:
    1. `client.clock(&root, SyncTimeout::Default).await` → store `ClockSpec`
    2. `client.state_enter(&root, "atomic-turn", SyncTimeout::Default, Some(json!({"session": session_id})))` — other subscribers can defer around agent turns
  - [ ] `end_turn()`:
    1. `client.state_leave(&root, "atomic-turn", SyncTimeout::Default, None)`
    2. `client.query(&root, QueryRequestCommon { since: Some(turn_start_clock), expression: Some(ignore_expr), ... })` — returns exactly the files that changed during this turn
    3. Classify files using Watchman `new` field (added) and `exists` field (deleted vs modified)
    4. Return `TurnChanges { modified, added, deleted, timestamp }`
  - [ ] `cancel_turn()` — `state_leave` without querying
  - [ ] `is_active()` — `turn_start_clock.is_some()`
- [ ] Build the ignore expression: `Expr::Not(Box::new(Expr::DirName(DirNameTerm { path: ".atomic", ... })))` — exclude `.atomic/` directory from all results
- [ ] Unit tests: begin/end lifecycle, clock capture, expression building, file classification

### 16.4 Background Subscription (for live IDE integration)
_Optional — uses Watchman `subscribe` for real-time "agent is modifying these files" notification_

- [ ] Create `atomic-agent/src/watcher/subscription.rs`
  - [ ] `FileSubscription` struct wrapping `watchman_client::Subscription<NameOnly>`
  - [ ] `FileSubscription::start(conn: &WatchmanConnection) -> Result<Self>`:
    - Subscribe with `defer: ["atomic-turn"]` — notifications buffered during active turns, delivered after `state_leave`
  - [ ] `FileSubscription::next() -> Result<SubscriptionEvent>`:
    - Map `SubscriptionData::FilesChanged` → file list
    - Map `SubscriptionData::StateEnter` / `StateLeave` → turn boundary events
  - [ ] `FileSubscription::cancel() -> Result<()>`
  - [ ] Unit tests: subscription lifecycle, event mapping

### 16.5 Fallback Watcher (no Watchman) ✅
_Graceful degradation when Watchman daemon isn't running_

- [x] Create `atomic-agent/src/watcher/fallback.rs`
  - [x] `FallbackWatcher` struct: `config: WatcherConfig`, `pre_snapshot: Option<FileSnapshot>`, `active_session: Option<String>`
  - [x] `FileSnapshot` type: `HashMap<PathBuf, FileEntry>` where `FileEntry` has `mtime: SystemTime` + `size: u64`
  - [x] `take_snapshot(repo_root, ignore_patterns)` — walks directory tree via `walkdir`, skips ignored dirs (`.atomic`, `.git`, `node_modules`, `target`, `__pycache__`), captures relative paths with mtime+size
  - [x] `diff_snapshots(before, after) -> TurnChanges` — classifies modified (mtime or size changed), added (in after only), deleted (in before only), sorted deterministically
  - [x] Implement `FileWatcher` for `FallbackWatcher`:
    - `begin_turn()`: `take_snapshot()` → store as `pre_snapshot`
    - `end_turn()`: `take_snapshot()` again → `diff_snapshots()` → return `TurnChanges`, clear state
    - `cancel_turn()`: clear `pre_snapshot` and `active_session`, no-op if not active
    - `is_active()`: `pre_snapshot.is_some()`
  - [x] Custom ignore patterns via `WatcherConfig::with_ignore_pattern()`
  - [x] Works as `Box<dyn FileWatcher>` trait object
  - [x] Unit tests: 33 tests (snapshot empty/nested/ignore/.git, diff no-changes/added/deleted/modified-mtime/modified-size/mixed/sorted/both-empty, watcher lifecycle active/inactive/cancel/end-without-begin, change detection added/deleted/modified/mixed/ignore/.atomic/nested/multiple-turns/overwrite-begin, custom ignore patterns, trait object)

---

## Phase 17: Turn State Machine

Direct port of Entire's `session/phase.go`, but actions resolve to Atomic operations instead of git shadow branch operations.

### 17.1 Phase / Event / Action Model ✅
_Replaces: `cli/cmd/entire/cli/session/phase.go` (320 lines Go → ~300 lines Rust + 500 lines tests)_

- [x] Create `atomic-agent/src/turn/mod.rs` — module declarations, re-exports
- [x] Create `atomic-agent/src/turn/phase.rs`
  - [x] `Phase` enum: `Idle`, `Active`, `ActiveRecorded`, `Ended`
    - `is_active()`, `is_ended()`, `is_idle()`, `from_str_normalized()`, `as_str()`
    - Default: `Idle`, Serde roundtrip, Display
  - [x] `Event` enum: `TurnStart`, `TurnEnd`, `Recorded`, `SessionStart`, `SessionStop`
    - `as_str()`, Display (PascalCase for readability)
  - [x] `Action` enum: `RecordTurn`, `RecordIfChanged`, `DiscardIfNoFiles`, `UpdateInteraction`, `ClearEndedAt`, `WarnStaleSession`
    - `is_strategy_specific()`, `as_str()`, Display
    - ~~`MigrateShadowBranch`~~ — **deleted**, no shadow branches in Atomic
  - [x] `TransitionContext` struct: `has_files_changed: bool`
    - `with_files_changed()` constructor, Default
    - ~~`IsRebaseInProgress`~~ — **deleted**, Atomic doesn't have rebase
  - [x] `TransitionResult` struct: `new_phase`, `actions`
    - `is_noop()`, `requires_recording()`, `strategy_actions()`, Display
  - [x] `transition()` — pure function, no side effects
  - [x] `SessionState` trait: `set_phase()`, `touch_interaction()`, `clear_ended_at()`
  - [x] `apply_common_actions()` — applies common actions to session state, returns strategy-specific remainder
  - [x] Transition table (same as Entire's minus rebase/shadow-branch paths):

      | From | Event | To | Actions |
      |---|---|---|---|
      | Idle | TurnStart | Active | UpdateInteraction |
      | Idle | TurnEnd | Idle | _(no-op)_ |
      | Idle | Recorded | Idle | RecordTurn, UpdateInteraction |
      | Idle | SessionStart | Idle | _(no-op)_ |
      | Idle | SessionStop | Ended | UpdateInteraction |
      | Active | TurnStart | Active | UpdateInteraction _(Ctrl-C recovery)_ |
      | Active | TurnEnd | Idle | UpdateInteraction |
      | Active | Recorded | ActiveRecorded | UpdateInteraction |
      | Active | SessionStart | Active | WarnStaleSession |
      | Active | SessionStop | Ended | UpdateInteraction |
      | ActiveRecorded | TurnStart | Active | UpdateInteraction _(Ctrl-C recovery)_ |
      | ActiveRecorded | TurnEnd | Idle | RecordTurn, UpdateInteraction |
      | ActiveRecorded | Recorded | ActiveRecorded | UpdateInteraction |
      | ActiveRecorded | SessionStart | ActiveRecorded | WarnStaleSession |
      | ActiveRecorded | SessionStop | Ended | UpdateInteraction |
      | Ended | TurnStart | Active | ClearEndedAt, UpdateInteraction |
      | Ended | TurnEnd | Ended | _(no-op)_ |
      | Ended | Recorded (files) | Ended | RecordIfChanged, UpdateInteraction |
      | Ended | Recorded (no files) | Ended | DiscardIfNoFiles, UpdateInteraction |
      | Ended | SessionStart | Idle | ClearEndedAt |
      | Ended | SessionStop | Ended | _(no-op)_ |

  - [x] Unit tests: 65 tests covering:
    - Phase/Event/Action basics (is_active, display, serde, as_str roundtrip)
    - All 20 phase×event transitions (×2 context variants = 40 combinations verified)
    - Context isolation (has_files_changed only affects Ended+Recorded)
    - Full lifecycle integration: single turn, multi-turn, mid-turn record, session re-entry, Ctrl-C recovery
    - `apply_common_actions` with MockSession: phase update, interaction touch, ended_at clear, strategy action passthrough
    - Exhaustive transition table validation (all produce valid phases, noop consistency)

### 17.2 Session State Persistence ✅
_Replaces: `cli/cmd/entire/cli/session/state.go` (350 lines Go → ~300 lines Rust + 500 lines tests)_

- [x] Create `atomic-agent/src/turn/session.rs`
  - [x] `AgentSession` struct (serialized as JSON):
    - `session_id`, `stack_name` (format: `agent/{session_id}`), `phase`, `turn_count`
    - `agent_name`, `agent_display_name`, `agent_vendor`, `model`
    - `started_at`, `last_interaction`, `ended_at`
    - `transcript_path`, `first_prompt` (truncated to 200 chars)
    - `files_touched` (deduplicated), `current_turn_started_at`
  - [x] `AgentSession::new()`, `make_stack_name()`, `set_model_info()`, `set_transcript_path()`, `set_first_prompt()`
  - [x] `add_files_touched()` with dedup, `files_touched_count()`, `begin_turn()`, `end_turn()` → returns turn number
  - [x] `current_turn_duration_ms()`, `is_ended()`, `is_turn_active()`, `duration_display()`
  - [x] `impl SessionState for AgentSession` — `set_phase()` auto-sets `ended_at`, `touch_interaction()`, `clear_ended_at()`
  - [x] Serde backward compatibility: missing optional fields default gracefully
  - [x] `SessionStore` struct: `sessions_dir: PathBuf`
  - [x] `SessionStore::new(sessions_dir)`, `for_repo(repo_root)` — creates `.atomic/sessions/` directory
  - [x] `SessionStore::load(session_id)` — returns `Ok(None)` for missing, validates ID
  - [x] `SessionStore::save(session)` — atomic write (temp file + rename)
  - [x] `SessionStore::list()` — sorted newest-first, skips `.tmp` files and corrupted JSON with warning
  - [x] `SessionStore::clear(session_id)` — idempotent delete
  - [x] `SessionStore::find_active()`, `find_ended()`, `count()`
  - [x] `validate_session_id()` — rejects empty, `..`, `/`, `\`, `\0`, >255 chars
  - [x] Unit tests: 49 tests (construction, model info, transcript path no-overwrite, first prompt truncation/no-overwrite, files_touched dedup, turn lifecycle, SessionState trait impl, serde roundtrip + backward compat, validation, store save/load/overwrite/path-traversal, clear, list sorted/skip-tmp/skip-corrupted, find_active/find_ended, count, for_repo, full lifecycle integration)

### 17.3 Turn Orchestrator ✅
_Replaces: `hooks_claudecode_handlers.go` (~870 lines) + `manual_commit*.go` (~2000 lines), collapsed because Atomic IS the storage_

- [x] Create `atomic-agent/src/turn/orchestrator.rs`
  - [x] `DispatchResult` struct: `session_id`, `new_phase`, `change_recorded: Option<TurnRecordOutcome>`, `message: Option<String>`, `warnings: Vec<String>`
    - `was_recorded()`, Display impl, builder methods `with_change()`, `with_message()`, `with_warning()`
    - `DispatchResult::new()` public constructor for external test use
  - [x] `TurnOrchestrator` struct: `repo_root`, `session_store`, `watcher: Box<dyn FileWatcher>`
  - [x] `TurnOrchestrator::new(repo_root)` — creates SessionStore + FileWatcher (auto-detect)
  - [x] `TurnOrchestrator::with_watcher(repo_root, session_store, watcher)` — for testing with injected watcher
  - [x] `dispatch(event: TurnEvent) -> AgentResult<DispatchResult>` — main entry point, routes by `event_type`
  - [x] `handle_session_start()`:
    - Load existing session → re-enter (transition SessionStart, apply_common_actions, handle WarnStaleSession)
    - Or create new AgentSession, set transcript path, save, return tracking message
  - [x] `handle_turn_start()`:
    - `load_or_create_session()` (resilient to missing state)
    - Store prompt (`set_first_prompt`), transcript path
    - `watcher.begin_turn()` (logs warning on failure, continues)
    - `session.begin_turn()`, transition TurnStart, save
  - [x] `handle_turn_end()`:
    - `watcher.end_turn()` → `TurnChanges` (empty on failure)
    - `session.end_turn()` → turn_number, `current_turn_duration_ms()`
    - Transition TurnEnd with `has_files_changed` context
    - Execute `RecordTurn`/`RecordIfChanged`: call `record_turn()`, log outcome or error as warning
    - Execute `DiscardIfNoFiles`: log info
    - `add_files_touched()`, save session
  - [x] `handle_session_end()`:
    - Cancel active watcher turn if session was active
    - Transition SessionStop, save
    - Graceful for unknown sessions (log + return Ended)
  - [x] `handle_tool_use()`:
    - Log tool name for info, return current phase
    - Sub-turn recording designed but not yet implemented (see Known Limitations)
  - [x] `load_or_create_session()` — resilient helper, creates new session if state file missing
  - [x] Unit tests: 19 tests (DispatchResult basics, session start creates/resumes, turn start transitions/auto-creates session, turn end with no changes/with file changes, session end transitions/unknown-ok/cancels-active-watcher, tool use handled/unknown-ok, full multi-turn lifecycle, Ctrl-C recovery, Debug trait)

---

## Phase 18: Turn-Level Recording

### 18.1 Turn → Atomic Change ✅
_Uses `atomic-repository`'s existing `record()` with provenance — no new VCS code needed_

- [x] Create `atomic-agent/src/record.rs`
  - [x] `TurnRecordOptions` struct: `session`, `changes`, `event`, `turn_number`, `turn_duration_ms`, `prompt`
  - [x] `TurnRecordOutcome` struct: `hash`, `turn_number`, `file_count`, `message` (with Display impl)
  - [x] `build_turn_header()`:
    - Message format: `"Turn {n}: {prompt_72chars}"` or `"Turn {n} ({agent_display_name})"`
    - Author: `Author::new(session.agent_display_name, None)`
  - [x] `build_turn_provenance()`:
    - `vendor`: from `session.agent_vendor` or inferred from agent name (`claude-code` → Anthropic, etc.)
    - `model`: from session or `"unknown"` fallback
    - `tool`: `AITool::Cli(session.agent_name)`
    - `suggestion_type`: `SuggestionType::Complete`
    - `prompt`: `PromptContent::Hashed(Hash::of(prompt))` — privacy-aware, no raw text
    - `session_id`: `Some(session.session_id)`
    - `metadata`: includes `("turn_number", "N")` and `("agent_name", "...")` key-value pairs
    - `timestamp`: from event
  - [x] `build_turn_envelope()`:
    - Populates `SessionEnvelope` with turn context, timing, files, prompt summary + hash
    - Conditionally sets prompt fields (skips when None)
  - [x] `record_turn(repo_root, options)`:
    - Rejects empty turns (`AgentError::EmptyTurn`)
    - Builds header + provenance + envelope
    - Encodes envelope bytes (validated end-to-end)
    - Opens repository, calls `repo.record(header, record_options)` with paths + stack + provenance
    - Returns `TurnRecordOutcome`
  - [x] `truncate_prompt()` helper: trims whitespace, adds "..." if over limit, Unicode-safe
  - [x] `vendor_from_agent_name()` helper: claude-code→Anthropic, gemini-cli→Google, codex→OpenAI
  - [x] **Known limitation**: SessionEnvelope bytes computed and validated but not yet persisted in `HashedChange.metadata` — `RecordOptions` does not accept `metadata_bytes` field (see Phase 18.5.4). Provenance IS included in hash.
  - [x] Unit tests: 40 tests (truncate_prompt short/exact/long/whitespace/unicode, message with/without/long prompt, header message+author, vendor inference all agents, provenance vendor/model/tool/session_id/prompt-hash/no-prompt/suggestion-type/metadata/vendor-fallback/model-fallback/timestamp, envelope session_id/agent/turn_number/duration/files_in_turn/files_in_session/prompt_summary/prompt_hash/no_prompt/session_started_at/encode-roundtrip, record_turn empty-rejected/nonexistent-repo-fails, outcome display singular/plural)

### 18.2 Transcript Metadata Storage
_Replaces: Entire's `entire/checkpoints/v1` orphan branch. Transcript stored in `HashedChange.metadata`._

- [ ] Create `atomic-agent/src/transcript.rs`
  - [ ] `TranscriptMetadata` struct (serde):
    - `session_id: String`
    - `turn_number: u32`
    - `prompt: Option<String>`
    - `prompt_hash: String` — Blake3 hash
    - `files_touched: Vec<String>`
    - `token_usage: Option<TokenUsage>`
    - `compressed_transcript: Option<Vec<u8>>` — zstd-compressed transcript bytes
    - `transcript_format: String` — "jsonl" (Claude) or "json" (Gemini)
  - [ ] `TranscriptMetadata::serialize() -> Result<Vec<u8>>` — bincode encoding
  - [ ] `TranscriptMetadata::deserialize(data: &[u8]) -> Result<Self>`
  - [ ] `TranscriptMetadata::compress_transcript(raw: &[u8]) -> Vec<u8>` — zstd compression
  - [ ] `TranscriptMetadata::decompress_transcript(compressed: &[u8]) -> Result<Vec<u8>>`
  - [ ] `extract_transcript(change: &Change) -> Option<TranscriptMetadata>` — read from change metadata
  - [ ] `attach_transcript(change: &mut Change, metadata: TranscriptMetadata)` — write into change metadata
  - [ ] Unit tests: serialize/deserialize roundtrip, compression/decompression, extract from change

### 18.3 Token Usage Tracking
- [ ] Create `atomic-agent/src/tokens.rs`
  - [ ] `TokenUsage` struct (mirrors Entire's `agent.TokenUsage`):
    - `input_tokens: u64`
    - `cache_creation_tokens: u64`
    - `cache_read_tokens: u64`
    - `output_tokens: u64`
    - `api_call_count: u32`
    - `subagent_tokens: Option<Box<TokenUsage>>`
  - [ ] `TokenUsage::accumulate(&mut self, other: &TokenUsage)` — running total across turns
  - [ ] `TokenUsage::total_tokens() -> u64`
  - [ ] `TokenUsage::estimated_cost(pricing: &Pricing) -> f64` — optional cost estimate
  - [ ] Unit tests: accumulation, total calculation

### 18.5 Session-in-Change Architecture
_Session data lives INSIDE the change so it commutes in the DVCS, pushes to the server, and renders in UI._

Atomic changes already have three data slots. Session data maps onto them precisely:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    Change File: Three Slots for Session Data                │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  SLOT 1: hashed.provenance (Vec<Provenance>)                               │
│  ──────────────────────────────────────────                                 │
│  Already exists. Already hashed. Already has session_id.                    │
│  Per-TURN data: vendor, model, tokens, cost, prompt hash, suggestion type  │
│  ✓ Part of change hash → tamper-evident                                    │
│  ✓ Commutes with change → arrives on server via push                       │
│  ✓ Server reads provenance to render turn-level metrics                    │
│                                                                             │
│  SLOT 2: hashed.metadata (Vec<u8>)                                         │
│  ─────────────────────────────────                                          │
│  Already exists. Already hashed. Currently opaque bytes.                    │
│  Per-TURN session envelope: turn number, session context, files touched     │
│  ✓ Part of change hash → tamper-evident                                    │
│  ✓ Structured via new SessionEnvelope type (bincode-encoded)               │
│  ✓ Server deserializes to build session timeline UI                        │
│                                                                             │
│  SLOT 3: unhashed (Option<serde_json::Value>)                              │
│  ────────────────────────────────────────────                               │
│  Already exists. NOT hashed. JSON blob.                                     │
│  TRANSCRIPT data: full conversation, large/privacy-sensitive               │
│  ✓ Does NOT affect change identity → safe to strip for public repos        │
│  ✓ Pushed/pulled WITH the change → server has it for UI                    │
│  ✓ Can be redacted without invalidating the change hash                    │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

**Why this works for commutation and push:**

Changes are the atoms of Atomic's DVCS. They are content-addressed, pushed as files
to remotes, pulled by other clients, and applied to stacks. Session data embedded in
the change travels with it automatically — no separate sync, no side-channel, no
metadata branch. When two developers push changes from different agent sessions,
the changes (with their session data) compose via patch theory exactly like any
other changes. The server receives the change files and can read session data
directly from provenance + metadata + unhashed fields to render UI.

```
Developer A (Claude Code)          Developer B (Gemini CLI)
        │                                   │
   Turn 1 → Change A1                  Turn 1 → Change B1
     provenance: anthropic/claude        provenance: google/gemini
     metadata: {turn:1, session:A}       metadata: {turn:1, session:B}
     unhashed: {transcript: ...}         unhashed: {transcript: ...}
        │                                   │
   Turn 2 → Change A2                  Turn 2 → Change B2
        │                                   │
        └──────── push ─────────┐   ┌────── push ──────┘
                                │   │
                                ▼   ▼
                         ┌──────────────┐
                         │    Server    │
                         │              │
                         │  Has ALL     │
                         │  session     │
                         │  data from   │
                         │  A1,A2,B1,B2 │
                         │  inside the  │
                         │  changes     │
                         │  themselves  │
                         └──────────────┘
                                │
                          Server UI reads:
                          • provenance → token/cost dashboards
                          • metadata → session timelines
                          • unhashed → transcript viewer
```

#### 18.5.1 `SessionEnvelope` Type (goes in `hashed.metadata`) ✅
_Implemented in `atomic-agent/src/envelope.rs` (not `atomic-core` — keeps agent-specific types in the agent crate)_

- [x] Create `atomic-agent/src/envelope.rs`
  - [x] `SessionEnvelope` struct (serde + bincode):
    - `schema_version: u8` — for forward compat (start at 1)
    - `session_id: String` — links turns within a session
    - `agent_name: String`, `agent_display_name: Option<String>`
    - `turn_number: u32`, `total_turns: Option<u32>` — backfilled on session end
    - `session_started_at: i64`, `turn_started_at: i64`, `turn_ended_at: i64`, `turn_duration_ms: u64`
    - `prompt_summary: Option<String>`, `prompt_hash: Option<[u8; 32]>` — Blake3 of full prompt
    - `files_in_turn: Vec<String>`, `files_in_session: u32`
    - `delegation_id: Option<String>` — identity delegation reference
  - [x] `SessionEnvelopeBuilder` — fluent builder with `prompt_hash_from_text()` (computes Blake3)
  - [x] Wire format: `[MAGIC "ATSE": 4 bytes][bincode payload]` — magic enables fast `is_session_envelope()` check
  - [x] `encode() -> AgentResult<Vec<u8>>` — magic + bincode serialize
  - [x] `decode(data: &[u8]) -> AgentResult<Self>` — validates magic, version, deserializes
  - [x] `is_session_envelope(data: &[u8]) -> bool` — 4-byte magic check, O(1)
  - [x] Helper methods: `turn_file_count()`, `has_prompt()`, `is_session_complete()`, `duration_display()`
  - [x] Display impl: "Turn 3 of session sess-abc (claude-code, 12.4s) — \"Fix the auth...\""
  - [x] Unit tests: 55 tests (builder, encode/decode roundtrip full/minimal/with-hash, decode errors: too short/wrong magic/unsupported version/corrupted, is_session_envelope vs other data, duration display ranges, JSON roundtrip, schema versioning, edge cases: empty session_id, 1000 files, unicode prompts, u32::MAX turn number)

#### 18.5.2 Extend `Provenance` for Turn-Level Linking
- [ ] Add fields to `Provenance` in `atomic-core/src/change/provenance.rs`:
  - `turn_number: Option<u32>` — which turn in the session (links to SessionEnvelope)
  - `agent_name: Option<String>` — "claude-code" etc. (redundant with SessionEnvelope but useful when metadata isn't present)
  - Note: `session_id: Option<String>` already exists — use it
- [ ] Update `ProvenanceBuilder` to support new fields
- [ ] Update serialization tests
- [ ] Ensure backward compat: new fields are `#[serde(default)]`

#### 18.5.3 Transcript + Reasoning in `unhashed` Section
_Replaces: Entire CLI's `summarize/` package + transcript storage on `entire/checkpoints/v1` branch._

The `unhashed` section of each change carries two things:

1. **Condensed transcript** — the raw conversation (prompts, assistant responses, tool calls)
2. **AI-generated reasoning summary** — structured analysis: intent, outcome, learnings, friction, open items

Both are unhashed so they can be redacted from public repos without invalidating the change.
The transcript is the raw record; the reasoning is a structured distillation of it.

**Entire CLI reference**: `cli/cmd/entire/cli/summarize/summarize.go` condenses transcripts into
`[User] / [Assistant] / [Tool]` entries, then `claude.go` calls Claude CLI with a summarization
prompt to generate a `Summary` struct with `intent`, `outcome`, `learnings` (repo/code/workflow),
`friction`, and `open_items`. This is stored in `metadata.json` on the checkpoints branch.

In Atomic, both live in `change.unhashed` — no separate branch, no separate sync.

##### 18.5.3a Condensed Transcript
- [ ] Create `atomic-agent/src/transcript.rs`
- [ ] `CondensedEntry` struct:
  - `entry_type: EntryType` — `User`, `Assistant`, `Tool`
  - `content: Option<String>` — text content for user/assistant entries
  - `tool_name: Option<String>` — for tool entries
  - `tool_detail: Option<String>` — description, file path, command, or URL
- [ ] `condense_transcript(raw_bytes: &[u8], format: &str) -> Result<Vec<CondensedEntry>>`:
  - Parse JSONL (Claude) or JSON (Gemini) transcript format
  - Extract user prompts, assistant text responses, and tool calls
  - Filter out noise: skill content injections, verbose tool outputs (Read file contents, WebFetch bodies)
  - Minimal detail for read-heavy tools: show path/URL only, not content
- [ ] `format_condensed(entries: &[CondensedEntry], files: &[String]) -> String`:
  - Format as `[User] prompt\n[Assistant] response\n[Tool] ToolName: detail\n`
  - Append `[Files Modified]\n- path\n` section
  - This is the human-readable form stored in the unhashed section and fed to the reasoning generator

##### 18.5.3b AI-Generated Reasoning Summary
_Mirrors Entire CLI's `checkpoint.Summary` struct and `ClaudeGenerator`._

The reasoning summary captures what the agent learned during a turn — intent, outcome,
discoveries, problems, and deferred work. Developers see learnings in terms of files,
functions, and lines. Under the hood, Atomic's CRDT semantic layer (FileOps/LeafOps)
anchors each finding to the specific tokens in the graph, so learnings automatically
follow the code through renames, refactors, and merges without user involvement.

**What developers see:**

```
atomic log --learnings

  Turn 3: Fix the auth bug in login.rs
  ├── Intent: Fix authentication bug preventing user login
  ├── Outcome: Fixed token validation, all tests passing
  ├── Learnings:
  │   ├── src/auth/login.rs:42 — Token expiry compared with wrong timezone
  │   ├── Repo: Auth module uses JWT with RS256, not HS256
  │   └── Workflow: cargo test --lib is faster for auth changes
  ├── Friction: 3 layers of middleware to find the validation
  └── Open: Refresh endpoint has the same bug (deferred)
```

Six months later, after refactors move that code to `src/auth/v2/session.rs` line 87,
the same learning still shows up in the right place — because the CRDT graph tracks
the tokens, not the line numbers. The user never thinks about this. It just works.

**How it stays accurate (invisible to users):**

When `record_turn()` creates a change, the change includes `FileOps` — CRDT operations
that assign stable IDs to every file (`TrunkId`), line (`BranchId`), and token (`LeafId`)
the agent touched. After the reasoning generator returns human-readable findings with
file paths and line numbers, `anchor_to_graph()` silently attaches the corresponding
CRDT IDs. The user-facing data never changes. The graph references ride along as hidden
metadata that the server uses for resolution.

- [ ] `TurnReasoning` struct (serde, stored in unhashed JSON):
  - `intent: String` — what the user was trying to accomplish (1-2 sentences)
  - `outcome: String` — what was actually achieved (1-2 sentences)
  - `learnings: Learnings` — categorized discoveries
  - `friction: Vec<String>` — problems, blockers, annoyances encountered
  - `open_items: Vec<String>` — tech debt, unfinished work, things to revisit
- [ ] `Learnings` struct:
  - `repo: Vec<String>` — codebase-specific patterns, conventions, gotchas
  - `code: Vec<CodeLearning>` — findings about specific code locations
  - `workflow: Vec<String>` — general dev practices, tool usage insights
- [ ] `CodeLearning` struct (user-facing fields):
  - `path: String` — file path (e.g., `"src/auth/login.rs"`)
  - `line: Option<u32>` — line number
  - `end_line: Option<u32>` — end line for ranges
  - `function: Option<String>` — function/method name if identifiable
  - `finding: String` — what was learned, in plain language
  - `category: Option<String>` — "bug", "pattern", "convention", "performance", "security"
- [ ] `CodeLearning` struct (graph anchor fields — set by `anchor_to_graph()`, invisible to users):
  - `_anchor: Option<GraphAnchor>` — serialized but never shown in CLI/UI output
- [ ] `GraphAnchor` struct (internal):
  - `trunk: (u64, u32)` — TrunkId (file) — survives renames
  - `branches: Vec<(u64, u32)>` — BranchIds (lines) — survive insertions above
  - `leaves: Vec<(u64, u32)>` — LeafIds (tokens) — survive refactors
  - These are populated from the change's `FileOps` after recording
  - The server uses these to resolve learnings to current file/line positions
  - If the code moves, the server resolves the anchor and updates the display position
- [ ] `anchor_to_graph(learnings: &mut [CodeLearning], file_ops: &[FileOps])`:
  - Post-processing step after the LLM returns findings
  - Maps each `CodeLearning.path + line` to the `TrunkId`/`BranchId`/`LeafId` from the change
  - If a finding references a line that wasn't changed in this turn, anchor is left empty
    (the finding is about context the agent read, not code it wrote)
  - This is a best-effort enrichment — learnings work fine without anchors (just no auto-tracking)
- [ ] `ReasoningGenerator` trait:
  - `fn generate(&self, condensed: &str, files: &[String]) -> Result<TurnReasoning>`
  - Allows pluggable backends (Claude CLI, local model, API)
- [ ] `ClaudeCliGenerator` struct (default implementation):
  - Calls `claude --print --output-format json --model sonnet --setting-sources ""`
  - Passes condensed transcript in `<transcript>` tags
  - Prompt asks for findings with file paths, line numbers, and function names
  - LLM returns human-readable JSON; `anchor_to_graph()` adds invisible graph refs after
  - Runs in isolated subprocess: `cmd.Dir = temp_dir`, strips `GIT_*` env vars
  - Non-blocking: generation failure logs warning, doesn't prevent recording
- [ ] Config: `strategy_options.summarize.enabled` in `.atomic/config.toml`
  - Default: `false` (opt-in, requires Claude CLI installed)
  - When enabled, reasoning is generated on each `record_turn()` after recording succeeds

**What the server does with graph anchors (invisible to users):**

| What the user sees | What happens underneath |
|---|---|
| Learning shown at `login.rs:42` | Server resolves `GraphAnchor.branches` to current line position |
| File renamed to `session.rs` | `GraphAnchor.trunk` (TrunkId) still resolves — learning follows |
| Code moved to line 87 by refactor | `GraphAnchor.leaves` (LeafIds) still resolve — learning follows |
| "What do we know about this function?" | Server queries all learnings whose `GraphAnchor.branches` overlap the function's line range |
| Same pattern across sessions | Different changes reference the same LeafIds — learnings cluster automatically |

The user never sees TrunkId, BranchId, or LeafId. They see files, functions, and lines.
The graph keeps those references accurate as the code evolves.

##### 18.5.3c Unhashed Storage
- [ ] `UnhashedTurnData` struct (the top-level unhashed JSON):
  - `session_id: String`
  - `turn_number: u32`
  - `transcript_format: String` — "jsonl" (Claude), "json" (Gemini), "markdown" (other)
  - `condensed_transcript: Vec<CondensedEntry>` — structured entries
  - `condensed_text: String` — formatted human-readable text
  - `prompts: Vec<String>` — extracted user prompts for quick access
  - `tools_used: Vec<ToolUseSummary>` — aggregated tool usage
  - `reasoning: Option<TurnReasoning>` — AI-generated summary (if enabled)
  - `redacted: bool` — if true, transcript/reasoning were stripped
- [ ] `ToolUseSummary` struct:
  - `tool_name: String`
  - `invocation_count: u32`
  - `files_affected: Vec<String>`
- [ ] `attach_unhashed(change: &mut Change, data: UnhashedTurnData)`:
  - Serialize as JSON into `change.unhashed`
  - Nest under `"agent_turn"` key to avoid colliding with other unhashed uses
- [ ] `extract_unhashed(change: &Change) -> Option<UnhashedTurnData>`:
  - Read from `change.unhashed["agent_turn"]`
- [ ] `strip_unhashed(change: &mut Change)`:
  - Remove transcript and reasoning from unhashed (for public repos / privacy)
  - Set `redacted: true` in a minimal stub so server knows it was stripped
  - Change hash is UNAFFECTED (unhashed section doesn't contribute to hash)
- [ ] Unit tests: attach/extract roundtrip, strip preserves hash, large transcript handling,
  reasoning present/absent, redaction flag

##### What the server gets after push

```
change.unhashed["agent_turn"] = {
  "session_id": "60f5cbd2-aa23-40ee",
  "turn_number": 3,
  "transcript_format": "jsonl",
  "condensed_transcript": [
    {"entry_type": "User", "content": "Fix the auth bug in login.rs"},
    {"entry_type": "Assistant", "content": "I'll fix the authentication..."},
    {"entry_type": "Tool", "tool_name": "Edit", "tool_detail": "src/auth/login.rs"},
    {"entry_type": "Tool", "tool_name": "Bash", "tool_detail": "cargo test"},
    {"entry_type": "Assistant", "content": "The fix is applied and tests pass."}
  ],
  "prompts": ["Fix the auth bug in login.rs"],
  "tools_used": [
    {"tool_name": "Edit", "invocation_count": 1, "files_affected": ["src/auth/login.rs"]},
    {"tool_name": "Bash", "invocation_count": 1, "files_affected": []}
  ],
  "reasoning": {
    "intent": "Fix authentication bug that prevented users from logging in",
    "outcome": "Fixed token validation logic in login handler, all tests passing",
    "learnings": {
      "repo": ["Auth module uses JWT with RS256, not HS256"],
      "code": [
        {
          "path": "src/auth/login.rs",
          "line": 42,
          "function": "validate_token",
          "finding": "Token expiry was compared with wrong timezone — uses UTC internally but comparison was against local time",
          "category": "bug",
          "_anchor": {"trunk": [47, 0], "branches": [[47, 12]], "leaves": [[47, 34], [47, 35]]}
        }
      ],
      "workflow": ["Running cargo test --lib is faster than full test suite for auth changes"]
    },
    "friction": ["Had to read through 3 layers of middleware to find the actual validation"],
    "open_items": ["Token refresh endpoint has the same expiry comparison bug (deferred)"]
  },
  "redacted": false
}
```

The server renders:
- **Transcript viewer** — the condensed entries as a conversation timeline
- **Reasoning panel** — intent/outcome as a summary card, learnings as indexed knowledge
- **Search** — prompts and reasoning text are full-text searchable
- **Knowledge graph** — code learnings with file/line references link to blame/diff views

#### 18.5.4 Change Construction in `record_turn()` 🔄 Slots 1+2 Complete
_Provenance and SessionEnvelope are in the change hash. Unhashed transcript is wired but reasoning is on-demand via `explain`._

- [x] **Slot 1: Provenance** — fully populated in `record_turn()` via `RecordOptions::provenance()`. Vendor, model, tool, tokens, cost, prompt hash, session_id, turn_number all set. **Included in change hash.**
- [x] **Slot 2: SessionEnvelope → `hashed.metadata`** ✅ — envelope is built, encoded, and passed to `RecordOptions::metadata_bytes()` BEFORE recording. The bytes flow through `AssemblyOptions` → `AssemblyContext::finalize()` → `HashedChange.metadata`. **Included in change hash — tamper-evident.**
  - [x] Added `metadata_bytes: Vec<u8>` field to `RecordOptions` in `atomic-repository/src/record.rs`
  - [x] Added `metadata_bytes: Vec<u8>` field to `AssemblyOptions` in `atomic-core/src/record/workflow/assembly.rs`
  - [x] Updated `AssemblyContext::finalize()` to accept and set `metadata_bytes` on the change
  - [x] Updated `record_turn()` to build the envelope from status files and pass `envelope.encode()` via `RecordOptions::metadata_bytes()`
  - [x] Envelope is built before `repo.record()` so it's part of the hash computation
  - [ ] Unit test: verify `HashedChange.metadata` contains valid `SessionEnvelope` after recording
  - [ ] Unit test: verify change hash changes when envelope content changes (tamper-evident)
- [x] **Slot 3: Unhashed transcript** — condensed transcript is attached to `change.unhashed["agent_turn"]` after recording via `build_unhashed_turn_data()` + `attach_unhashed()`. Reasoning is NOT generated during recording (too slow, potentially recursive inside agent hooks). Use `atomic agent explain --save` to generate reasoning on demand and write it back.

#### 18.5.5 Session Reconstruction from Changes
_Server-side: given a set of changes, reconstruct the full session timeline_

- [ ] Create `atomic-agent/src/session_view.rs`
  - [ ] `SessionView` struct:
    - `session_id: String`
    - `agent_name: String`
    - `turns: Vec<TurnView>`
    - `total_tokens: TokenUsage`
    - `total_cost: Cost`
    - `total_duration_ms: u64`
    - `files_touched: Vec<String>` — deduplicated across all turns
    - `started_at: DateTime<Utc>`
    - `ended_at: Option<DateTime<Utc>>`
  - [ ] `TurnView` struct:
    - `turn_number: u32`
    - `change_hash: Hash`
    - `prompt_summary: Option<String>`
    - `files_changed: Vec<String>`
    - `tokens: TokenUsage`
    - `cost: Cost`
    - `duration_ms: u64`
    - `timestamp: DateTime<Utc>`
    - `has_transcript: bool`
  - [ ] `reconstruct_session(changes: &[Change]) -> Result<Vec<SessionView>>`:
    1. Scan all changes for `SessionEnvelope` in metadata
    2. Group by `session_id`
    3. Sort turns by `turn_number`
    4. Aggregate tokens, cost, files, duration
    5. Check `unhashed` for transcript availability
    6. Return one `SessionView` per unique session
  - [ ] `reconstruct_session_for_stack(repo: &Repository, stack: &str) -> Result<Vec<SessionView>>`:
    - Load all changes in stack
    - Call `reconstruct_session()`
  - [ ] Unit tests: single session, multiple sessions interleaved, missing turns, empty sessions

#### 18.5.6 Server API Surface (data contract for `atomic-api`)
_Defines the JSON shapes that `atomic-api` will serve to the UI_

- [ ] Create `atomic-agent/src/api_types.rs`
  - [ ] `SessionSummaryResponse` (JSON, for session list endpoint):
    ```
    {
      "session_id": "2026-01-15-abc123",
      "agent": "claude-code",
      "turns": 7,
      "total_tokens": { "input": 45000, "output": 12000 },
      "total_cost_usd": 0.342,
      "files_touched": ["src/main.rs", "src/lib.rs"],
      "started_at": "2026-01-15T10:30:00Z",
      "duration_seconds": 847,
      "stack": "agent/2026-01-15-abc123"
    }
    ```
  - [ ] `TurnDetailResponse` (JSON, for turn detail endpoint):
    ```
    {
      "turn_number": 3,
      "change_hash": "ABCDEF...",
      "prompt_summary": "Fix the authentication bug in...",
      "files_changed": ["src/auth.rs"],
      "tokens": { "input": 8500, "output": 2100 },
      "cost_usd": 0.063,
      "duration_ms": 12400,
      "has_transcript": true,
      "provenance": { "vendor": "anthropic", "model": "claude-sonnet-4" }
    }
    ```
  - [ ] `TranscriptResponse` (JSON, for transcript viewer endpoint):
    ```
    {
      "session_id": "...",
      "turn_number": 3,
      "format": "jsonl",
      "prompts": ["Fix the authentication bug..."],
      "tools_used": [{"tool": "Edit", "count": 3, "files": ["src/auth.rs"]}],
      "transcript": "..." (or null if redacted)
    }
    ```
  - [ ] These are just type definitions — `atomic-api` implements the actual HTTP endpoints
  - [ ] `impl From<SessionView> for SessionSummaryResponse`
  - [ ] `impl From<TurnView> for TurnDetailResponse`
  - [ ] Unit tests: serialization to JSON matches expected shapes

#### Design Decisions

**Q: Why not put everything in `hashed.metadata`?**
A: Transcripts are large (100KB–10MB). Hashing them means changing the transcript
changes the change hash. That makes it impossible to redact transcripts from public
repos without invalidating every change. The unhashed section exists precisely for
data that should travel with the change but not define it.

**Q: Why not put everything in `unhashed`?**
A: Session envelope (turn number, session ID, timing, file list) should be tamper-evident.
If a change claims "turn 3, 8500 tokens, modified auth.rs" that should be part of
the cryptographic identity. Otherwise someone could forge session metadata on the
server. The hashed metadata slot guarantees integrity.

**Q: Why not a separate "session change" with no hunks?**
A: That would work but adds complexity — you'd need dependency edges between session
changes and code changes, ordering constraints, and special handling during push/pull.
Embedding session data in the code change means one change = one turn = one atomic unit.
No ordering problems, no dangling references, no sync issues.

**Q: What about session-level aggregates (total tokens, total cost)?**
A: These are computed by `reconstruct_session()` from individual turn data. No
separate "session summary" change needed. The server aggregates on read, just like
a SQL GROUP BY. This avoids the stale-aggregate problem (what if a turn is unrecorded?).

**Q: How does the server know a change has session data?**
A: Check `change.hashed.metadata` → try `SessionEnvelope::decode()`. If schema_version
is recognized, it's a session turn. `change.hashed.provenance` with `session_id.is_some()`
is a secondary signal. The server can index these on push receipt.

**Q: What happens when session changes commute with human changes?**
A: Nothing special. A session turn is a regular Atomic change. If a human edits
`src/main.rs` and an agent turn also edits `src/main.rs`, they compose via patch
theory like any two changes. The session metadata is cargo — it doesn't affect
merge behavior. The server shows both the human change and the agent turn in the
timeline, each with its own attribution.

### 18.4 Agent Identity Integration
_Uses `atomic-identity`'s existing delegation system_

- [ ] Create `atomic-agent/src/identity.rs`
  - [ ] `create_agent_identity(agent_name: &str, vendor: &str) -> Result<Identity>`:
    - `IdentityType::Agent`
    - `IdentityUsage::Bot`
    - Name: `"{agent_name}-agent"`
  - [ ] `create_delegation(user: &Identity, agent: &Identity, repo_pattern: &str) -> Result<Delegation>`:
    - Permissions: `Record`, `ManageStacks`
    - Scope: repository pattern
  - [ ] `get_or_create_agent_identity(store: &IdentityStore, agent_name: &str) -> Result<Identity>`:
    - Check store for existing agent identity
    - Create new one if not found
    - Cache in identity store
  - [ ] `resolve_signing_identity(session: &AgentSession) -> Result<Option<Identity>>`:
    - Look up agent identity for this session
    - Return `None` if no identity configured (unsigned changes still work)
  - [ ] Unit tests: identity creation, delegation permissions, store cache hit/miss

---

## Phase 19: CLI Commands

### 19.1 `atomic agent` Subcommand Router ✅
- [x] Create `atomic-cli/src/commands/agent/mod.rs`
  - [x] `Agent` struct (clap `Args`) with `#[command(arg_required_else_help = true)]`
  - [x] `AgentCommands` enum: `Enable`, `Disable`, `Status`, `Hooks`
  - [x] Register in `atomic-cli/src/commands/mod.rs` (module + re-export)
  - [x] Register in `atomic-cli/src/main.rs` `Commands` enum + dispatch
  - [x] Unit tests: 1 test (variant names constructible)

### 19.2 `atomic agent enable` ✅
_Replaces: `entire enable`_

- [x] Create `atomic-cli/src/commands/agent/enable.rs`
  - [x] Args: `--agent <name>`, `--force`, `--all`
  - [x] Implementation:
    1. `find_repository_root()` — requires `.atomic/` directory
    2. Create `.atomic/sessions/` directory
    3. Auto-detect agent (checks `.claude/`, etc.) or validate `--agent` name
    4. Multi-agent support: `--all` installs for all detected, single auto-detect picks one, multiple detected prompts user
    5. Default to first available agent (claude-code) with warning if no agent directory found
    6. `--force`: uninstall then reinstall
    7. Idempotent: reports "already installed" and returns 0 if hooks present
    8. Prints summary with provenance/metadata/transcript features
  - [x] Unit tests: 5 tests (default construction, install to temp repo, force reinstall roundtrip)

### 19.3 `atomic agent disable` ✅
_Replaces: `entire disable`_

- [x] Create `atomic-cli/src/commands/agent/disable.rs`
  - [x] Args: `--agent <name>`, `--all`
  - [x] Implementation:
    1. `find_repository_root()`
    2. Auto-detect installed agents or use `--agent`/`--all`
    3. `agent.uninstall(repo_root)` for each selected agent
    4. Reports "no hooks installed" gracefully
    5. Preserves `.atomic/sessions/` (tells user to use `atomic agent sessions clean`)
  - [x] Unit tests: 5 tests (default construction, roundtrip uninstall, preserves non-Atomic hooks)

### 19.4 `atomic agent status` ✅
_Replaces: `entire status`_

- [x] Create `atomic-cli/src/commands/agent/status.rs`
  - [x] Args: `--verbose`
  - [x] Implementation:
    1. Shows installed agents (✓) and detected-but-not-installed agents (○)
    2. Active sessions: ● with session_id, agent, turn count, duration
    3. Ended sessions: ○ with same info, limited to 5 recent (all with `--verbose`)
    4. Verbose mode: stack name, phase, first prompt, files touched (top 5 + count), transcript path
    5. Summary line: total sessions, turns, files touched
    6. Graceful on empty state: suggests `atomic agent enable`
  - [x] Unit tests: 4 tests (default construction, verbose flag, empty session store, session store with active/ended)

### 19.5 `atomic agent sessions`
_Replaces: `entire explain` + session listing_

- [ ] Create `atomic-cli/src/commands/agent/sessions.rs`
  - [ ] Subcommands:
    - `list` — show all sessions (active and ended)
    - `show <session-id>` — show detail for one session including turn history
    - `clean` — remove ended sessions older than N days
  - [ ] `list` implementation:
    - `SessionStore::list()`
    - Show: session_id, agent, phase, turns, started_at, files_touched count
  - [ ] `show` implementation:
    - Load session
    - Show session details
    - `atomic log --stack agent/{session_id}` to show turn-level changes
  - [ ] `clean` implementation:
    - Remove old session state files
    - Optionally delete agent stacks
  - [ ] Tests: list formatting, show detail, clean threshold

### 19.6 `atomic agent hooks` (internal — called by agent hooks) ✅
_Replaces: `entire hooks claude-code <verb>`, `entire hooks gemini <verb>`_

- [x] Create `atomic-cli/src/commands/agent/hooks.rs`
  - [x] Hidden command (`#[command(hide = true)]`)
  - [x] Args: `agent_name: String`, `verb: String`
  - [x] Implementation:
    1. Read raw bytes from stdin
    2. `AgentRegistry::require(agent_name)` — validates agent exists
    3. `HookType::from_verb(verb)` — maps agent-specific verbs to common types
    4. `agent.parse_event(hook_type, input)` → `TurnEvent`
    5. `find_repository_root()` — locates `.atomic/`
    6. Creates single-threaded tokio runtime (`Builder::new_current_thread()`)
    7. `TurnOrchestrator::new(repo_root)` → `orchestrator.dispatch(event)` → `DispatchResult`
    8. Warnings → stderr, recording outcomes → stderr
    9. If `result.message` is Some: writes `{"systemMessage": "..."}` JSON to stdout (Claude Code reads this)
  - [x] Graceful error handling: tool use parse failures logged as warnings, not fatal
  - [x] Unit tests: 8 tests (struct fields, all Claude Code verbs → HookType, all Gemini CLI verbs → HookType, unknown verbs, registry has claude-code, require unknown fails, JSON response format, no-message produces no output)

---

## Phase 20: Rewind = Unrecord

Entire's rewind is ~500 lines of Go that restores files from shadow branch commit trees. In Atomic, rewind is just `unrecord` — already implemented.

### 20.1 Turn-Aware Unrecord UX
- [ ] Extend `atomic-cli/src/commands/revise.rs` or create `atomic-cli/src/commands/agent/rewind.rs`
  - [ ] `atomic agent rewind` — interactive picker showing turns from agent stack
  - [ ] Display: turn number, prompt summary, files changed, timestamp
  - [ ] Select a turn → `repo.unrecord()` all changes after that turn
  - [ ] Working copy restored to post-selected-turn state (existing unrecord behavior)
  - [ ] Tests: rewind to specific turn, rewind latest

### 20.2 Agent Stack Integration with `atomic log`
- [ ] Verify `atomic log --stack agent/{session_id}` works with existing log command
- [ ] Add turn metadata display (prompt, files, token usage) when viewing agent stacks
- [ ] If needed, extend log output to show provenance info when present
- [ ] Tests: log output for agent stacks

---

## Phase 21: New File Structure

```
atomic/
├── atomic-agent/                    # NEW CRATE
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                   # Crate root, re-exports
│       ├── error.rs                 # AgentError enum
│       ├── event.rs                 # HookType, TurnEvent, TurnChanges
│       ├── hooks/                   # Agent hook adapters
│       │   ├── mod.rs              # AgentHook trait, AgentRegistry
│       │   ├── claude_code.rs      # Claude Code adapter (~200 lines)
│       │   ├── gemini_cli.rs       # Gemini CLI adapter (~150 lines)
│       │   ├── codex.rs            # Codex adapter (~150 lines)
│       │   └── opencode.rs         # OpenCode adapter (~150 lines)
│       ├── watcher/                 # Watchman integration
│       │   ├── mod.rs              # FileWatcher trait, auto-detection
│       │   ├── watchman_watcher.rs # WatchmanConnection, WatchmanTurnWatcher
│       │   ├── subscription.rs     # Background file subscription (optional)
│       │   └── fallback.rs         # FallbackWatcher (no Watchman)
│       ├── turn/                    # Turn state machine
│       │   ├── mod.rs              # Module exports
│       │   ├── phase.rs            # Phase, Event, Action, transition()
│       │   ├── session.rs          # AgentSession, SessionStore
│       │   └── orchestrator.rs     # TurnOrchestrator dispatch
│       ├── record.rs               # Turn → Atomic change recording
│       ├── transcript.rs           # Transcript metadata serialization
│       ├── tokens.rs               # Token usage tracking
│       └── identity.rs             # Agent identity + delegation
├── atomic-cli/
│   └── src/commands/
│       └── agent/                   # NEW CLI COMMANDS
│           ├── mod.rs              # Subcommand router
│           ├── enable.rs           # atomic agent enable
│           ├── disable.rs          # atomic agent disable
│           ├── status.rs           # atomic agent status
│           ├── sessions.rs         # atomic agent sessions [list|show|clean]
│           ├── hooks.rs            # atomic agent hooks <agent> <verb> (internal)
│           └── rewind.rs           # atomic agent rewind (interactive)
└── Cargo.toml                       # Updated workspace members
```

## Storage Layout Changes

```
.atomic/
├── pristine/              # Graph database (existing)
├── changes/               # Content-addressed changes (existing)
├── config.toml            # Repository config (existing)
├── current_stack          # Active stack name (existing)
├── sessions/              # NEW — agent session state
│   ├── 2026-01-15-abc123.json   # Session state file
│   └── 2026-01-15-def456.json
└── working_copy_id        # Working copy state (existing)
```

No `.git/entire-sessions/`. No `entire/checkpoints/v1` branch. No shadow branches. Session state is a JSON file. Turn data is an Atomic change.

---

## Dependency Summary

| Phase | Depends On | New Crate Dependencies |
|---|---|---|
| 14 (Foundation) | — | `watchman_client`, `serde_json`, `chrono`, `thiserror` |
| 15 (Adapters) | 14 | — |
| 16 (Watchman) | 14 | `watchman_client` (already added in 14) |
| 17 (State Machine) | 14, 16 | — |
| 18 (Recording) | 14, 17 | `zstd` (already workspace dep), `bincode` (already workspace dep) |
| 19 (CLI) | 14, 15, 16, 17, 18 | — |
| 20 (Rewind) | 19 | — |

## Build Order (Critical Path)

```
14.1 → 14.2 → 14.3 → 15.1 ──┐
                               ├→ 19.1 → 19.2 → 19.3 → 19.4 → 19.5 → 19.6 → 20.1
16.1 → 16.2 → 16.3 → 16.5 ──┤
                               │
17.1 → 17.2 → 17.3 ──────────┤
                               │
18.1 → 18.2 → 18.3 → 18.4 ──┘

(16.4 is optional, can be done anytime after 16.2)
(15.2, 15.3, 15.4 can be done anytime after 14.3, in parallel)
(20.2 can be done anytime after 19.6)
```

## Minimum Viable Agent ✅ COMPLETE

All phases required for the minimum viable agent are implemented:

1. **14** — Crate scaffold, event types, hook trait ✅
2. **15.1** — Claude Code adapter ✅
3. **16.1 + 16.5** — FileWatcher trait + FallbackWatcher ✅ (Watchman backend 16.2–16.3 deferred)
4. **17.1 + 17.2 + 17.3** — State machine + session persistence + orchestrator ✅
5. **18.1 + 18.5.1** — Turn → change recording + SessionEnvelope ✅
6. **19.1 + 19.2 + 19.3 + 19.4 + 19.6** — CLI commands ✅

This gives you: `atomic agent enable` → work with Claude Code → each turn becomes a change → `atomic log` to see turn history → `atomic unrecord` to rewind.

## Known Limitations (recorded from code)

Two design notes in the codebase document work that is planned but not yet complete:

### ~~1. SessionEnvelope not yet in change hash~~ ✅ RESOLVED
`RecordOptions::metadata_bytes()` now threads the encoded `SessionEnvelope` through
`AssemblyOptions` → `AssemblyContext::finalize()` → `HashedChange.metadata`. The session
envelope (turn number, timing, files, agent name) is part of the change's cryptographic
identity — tamper-evident and commutable via patch theory.

### 2. Sub-agent sub-turn recording not implemented (`orchestrator.rs`)
**Location**: `atomic-agent/src/turn/orchestrator.rs` — `handle_tool_use()` function
**Issue**: PreToolUse and PostToolUse events are received and logged, but do not create sub-turn recordings. The design (snapshot at PreToolUse, diff at PostToolUse, create sub-turn change with sub-agent identity) is documented but not coded.
**Impact**: Sub-agent work (e.g., Claude Code's Task tool spawning a sub-agent) is captured as part of the parent turn, not as a separate change. This means sub-agent attribution is less granular.
**Fix**: Phase 17.3 enhancement — add `begin_sub_turn()` / `end_sub_turn()` to orchestrator.

### 3. Watchman backend not wired up (`watcher/mod.rs`)
**Location**: `atomic-agent/src/watcher/mod.rs` — `create_watcher()` function
**Issue**: `create_watcher()` always returns a `FallbackWatcher` (walkdir-based snapshots). The Watchman backend (`WatchmanTurnWatcher`) that would use `clock` + `since` queries for O(changed-files) detection is designed but not implemented.
**Impact**: File change detection is O(all files) per turn boundary instead of O(changed files). Fine for small-to-medium repos, slower for large repos (100K+ files).
**Fix**: Phase 16.2–16.3 — implement `WatchmanConnection` + `WatchmanTurnWatcher`, add Watchman-first fallback logic to `create_watcher()`.

## Current Test Count

**1,705 tests passing** across `atomic-agent` (433) + `atomic-cli` (1,272), 0 failures:

### atomic-agent (411 unit + 22 doc = 433 tests):
- `error.rs`: 22 tests (display, classification, suggestions, exit codes, conversions, Send+Sync)
- `event.rs`: 47 tests (HookType verbs/boundary/display/serde, TurnEvent builder/prompt/display/serde, TurnChanges merge/summary/paths/serde)
- `hooks/mod.rs`: 27 tests (MockAgent, AgentRegistry CRUD/detect/installed/iter, trait object safety)
- `hooks/claude_code.rs`: 52 tests (parse all formats, empty/invalid/missing fields, install/uninstall filesystem, preserve non-Atomic hooks, detect presence, full roundtrip)
- `watcher/mod.rs`: 10 tests (WatcherConfig builder/clone/debug, trait object safety, create_watcher returns fallback)
- `watcher/fallback.rs`: 33 tests (snapshot, diff, watcher lifecycle, change detection, ignore patterns, trait object)
- `turn/phase.rs`: 56 tests (Phase/Event/Action basics, all 20 transitions ×2 contexts, context isolation, lifecycle integration, apply_common_actions)
- `turn/session.rs`: 49 tests (construction, model info, transcript, prompt, files, turn lifecycle, SessionState trait, serde, validation, store CRUD, list/find, full lifecycle)
- `turn/orchestrator.rs`: 19 tests (DispatchResult, session start/resume, turn start/auto-create, turn end empty/with-changes, session end/unknown/cancel, tool use, full lifecycle, Ctrl-C recovery)
- `envelope.rs`: 47 tests (builder, encode/decode roundtrip, decode errors, is_session_envelope, duration display, Display, JSON roundtrip, schema versioning, edge cases)
- `record.rs`: 40 tests (truncate_prompt, build_turn_message, header, vendor inference, provenance fields, envelope fields, encode roundtrip, record_turn errors, outcome display)
- `lib.rs`: 9 integration tests (re-exports, cross-module usage, state machine, envelope encode/decode)

### atomic-cli agent commands (29 tests):
- `agent/mod.rs`: 1 test (variant names)
- `agent/enable.rs`: 5 tests (defaults, install to temp repo, force reinstall)
- `agent/disable.rs`: 5 tests (defaults, roundtrip, preserve non-Atomic hooks)
- `agent/status.rs`: 4 tests (defaults, verbose, session store operations)
- `agent/hooks.rs`: 8 tests (struct fields, verb mapping Claude+Gemini, registry, JSON response format)
- Remaining 6 tests in other `atomic-cli` modules referencing agent types

## Server UI Data Flow

Once changes containing session data are pushed, the server can render:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                     Server UI Rendering from Change Data                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  On push receipt, for each change:                                          │
│    1. Try SessionEnvelope::decode(change.hashed.metadata)                   │
│    2. If success → index as agent turn:                                     │
│       • session_id → session grouping                                       │
│       • turn_number → ordering                                              │
│       • agent_name → agent icon/badge                                       │
│       • files_in_turn → file-level attribution                              │
│       • turn_duration_ms → performance metrics                              │
│    3. Read change.hashed.provenance:                                        │
│       • tokens → usage dashboard                                            │
│       • cost → billing/budget tracking                                      │
│       • vendor + model → model comparison analytics                         │
│       • prompt_hash → dedup detection                                       │
│    4. Check change.unhashed["agent_transcript"]:                            │
│       • If present → transcript viewer link                                 │
│       • If redacted → show "transcript redacted" badge                      │
│       • prompts → searchable prompt history                                 │
│       • tools_used → tool usage analytics                                   │
│                                                                             │
│  UI Views powered by this data:                                             │
│    • Session Timeline — turns ordered by number, grouped by session         │
│    • Token Dashboard — input/output/cache tokens per session/day/agent      │
│    • Cost Tracker — spend per session, per model, per developer             │
│    • File Attribution — which files were touched by AI vs human             │
│    • Transcript Viewer — full conversation with prompt highlighting         │
│    • Agent Comparison — Claude vs Gemini vs Codex performance metrics       │
│    • Turn Diff View — click a turn to see its file changes + prompt         │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

No separate API for session data. No separate sync protocol. No metadata branch.
The same `push` that sends code changes sends session data. The same `pull` that
fetches code changes fetches session data. The server reads it from the changes
it already has.