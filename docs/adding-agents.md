# Adding a New AI Agent to Atomic

This document explains how to add support for a new AI coding agent (e.g.
Claude Code, OpenCode, Codex, Gemini CLI) to the Atomic ecosystem: how the
pieces fit together, what you have to build, and how to verify it.

It is written for someone who has never touched this part of the codebase.

## The three repositories

Agent support spans **three** repositories. Know which one owns which concern.

| Repo | Role | What lives here |
|------|------|-----------------|
| **`atomic`** (this repo) | The engine + CLI | The `atomic-agent` crate: per-agent *hook adapters*, the `AgentRegistry`, the verb→event map, and the `atomic agent …` CLI. This is where you write Rust to make Atomic *understand* an agent. |
| **`atomic-<agent>`** (e.g. `atomic-opencode`, `atomic-claude`) | The integration package | The agent-facing glue: the system prompt (`agents/*.md`, `CLAUDE.md`, …), the skills (`skills/*/SKILL.md`), the wiring that makes the agent call back into `atomic agent hooks …` (a plugin, or a hooks manifest), and an installer. Published to npm / installed via `install.sh`. |
| **`atomic-agents`** | The test harness | An ACP-based harness (`atomic-agent-harness`) that spawns each agent, sends real prompts, and asserts on the ACP stream and repository side effects. Also a native pure-Rust ACP agent (`atomic-agent`). This is where you register an agent so it gets *tested*. |

The mental model:

```
   atomic-<agent> package                atomic (this repo)
   ─────────────────────                 ──────────────────
   agent runs, fires lifecycle    ──►    atomic agent hooks <agent> <verb>
   events → calls the CLI                     │
   (plugin or native hooks)                   ▼
                                        AgentRegistry.require(<agent>)
                                        adapter.parse_event(verb, stdin-json)
                                             │
                                             ▼
                                        TurnEvent → TurnOrchestrator
                                             │
                                             ▼
                                        change recorded with AI provenance


   atomic-agents (harness)  ──► spawns the agent over ACP, prompts it,
                                verifies it can search / status / record
```

Two integration *styles* exist. **Which one(s) you can use is dictated by the
agent, not by preference** — it depends entirely on what extension mechanism
the target agent exposes:

1. **Native-hooks style** (Claude Code, Codex, Gemini, Cursor, …). The agent
   has its own hook system in a settings file. The integration package ships a
   **hooks manifest** JSON; `atomic agent enable --hooks <manifest>` merges it
   into the agent's settings file. See `atomic-claude`.
2. **Plugin style** (OpenCode, …). The agent exposes a plugin/extension API.
   The integration package ships a plugin that subscribes to lifecycle events
   and shells out to `atomic agent hooks <agent> <verb>`. See `atomic-opencode`
   (`plugins/atomic-hooks.ts`).

You do not freely pick between these — the agent constrains you:

- Some agents **only** support a plugin (no user-editable hooks config), so the
  plugin style is your only option — this is why OpenCode ships a plugin.
- Some agents **only** expose a settings/hooks file (no plugin API), so the
  native-hooks manifest is the only option — this is why Claude Code ships a
  manifest.
- Some agents support **both**, in which case pick whichever gives you richer
  lifecycle data (a plugin can usually report model/tokens/cost/timing that a
  shell hook cannot) and the simpler install.
- Some agents support **neither** cleanly. Then you're limited to whatever
  coarse signal is available (e.g. detecting a config directory and recording
  on a file-watch / session boundary) — and the adapter's `supported_hooks()`
  should advertise only the events you can actually observe.

Before writing anything, **investigate the target agent's extension model
first** and let that decide the style. Whichever style you land on, the payload
that reaches Atomic is identical: JSON on stdin to
`atomic agent hooks <agent> <verb>`.

---

## Part 1 — The hook adapter in `atomic` (required)

Everything in the `atomic agent hooks <agent> <verb>` pipeline is keyed off a
**string agent name**, dispatched through the `AgentRegistry`. To make Atomic
understand a new agent you implement one trait and register it.

### 1.1 Create the adapter file

Create `atomic-agent/src/hooks/<agent>.rs`. Use
`atomic-agent/src/hooks/opencode.rs` as the reference implementation — it is
the cleanest example and covers all six hook types.

Implement the `AgentHook` trait (`atomic-agent/src/hooks/mod.rs:131`):

```rust
pub trait AgentHook: Send + Sync + fmt::Debug {
    fn name(&self) -> &str;                 // registry key, e.g. "myagent"
    fn display_name(&self) -> &str;         // "My Agent"
    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent>;
    fn install(&self, repo_root: &Path) -> AgentResult<usize>;
    fn uninstall(&self, repo_root: &Path) -> AgentResult<()>;
    fn is_installed(&self, repo_root: &Path) -> bool;
    fn supported_hooks(&self) -> Vec<HookType>;
    fn hook_verbs(&self) -> Vec<&str>;

    // Optional (have defaults):
    fn detect_presence(&self, repo_root: &Path) -> bool { false }
    fn stdout_response(&self, hook_type: HookType) -> Option<&'static str> { None }
    fn repo_root_hints(&self, event: &TurnEvent) -> Option<Vec<PathBuf>> { None }
}
```

Key responsibilities of `parse_event`:

- Deserialize the agent's stdin JSON (each agent sends a different shape — model
  one struct per verb, as `opencode.rs` does).
- Build a `TurnEvent` (`atomic-agent/src/event.rs`) with `session_id`, and
  attach `prompt`, `tool_name`, `tool_use_id` where relevant.
- Stash anything else the orchestrator/provenance wants (model, provider,
  tokens, cost, finish reason) into `raw_json` — see how `opencode.rs`
  re-inserts `model`/`provider` into the raw JSON object.

The six canonical hook types (`atomic-agent/src/event.rs:67`) are:
`SessionStart`, `SessionEnd`, `TurnStart`, `TurnEnd`, `PreToolUse`,
`PostToolUse`. An agent need not support all of them — return only what it
emits from `supported_hooks()` and `hook_verbs()`.

Regarding `install`/`uninstall`/`is_installed`: if the integration package
handles installation itself (plugin style, or manifest style), these can be
near no-ops. OpenCode's adapter returns `Ok(1)`/`Ok(0)` from `install` by
checking whether the plugin file exists, and `uninstall` is a no-op — because
the `atomic-opencode` npm package owns installation. Only implement real
config-file writing here if you want `atomic agent enable --agent <name>` to be
the installer (the older per-repo path).

### 1.2 Register the adapter

Add the module and register it in the default registry
(`atomic-agent/src/hooks/mod.rs`):

```rust
// near line 59, with the other `pub mod` declarations
pub mod myagent;

// in AgentRegistry::with_defaults() near line 319
registry.register(Box::new(myagent::MyAgentHook::new()));
```

That is the *only* place that decides which agents exist. `atomic agent hooks`,
`enable`, `status`, and auto-detection all read from this registry.

### 1.3 Map the agent's verbs to hook types

`atomic agent hooks <agent> <verb>` turns `<verb>` into a `HookType` via
`HookType::from_verb` (`atomic-agent/src/event.rs:166`). This one function is
the union of *all* agents' verb strings. If your agent introduces a verb string
that is not already mapped, add an arm:

```rust
// in HookType::from_verb
"my-turn-begin" => Some(HookType::TurnStart),
"my-turn-done"  => Some(HookType::TurnEnd),
```

Many common verbs (`session-start`, `session-end`, `user-prompt`, `stop`,
`before-tool`, `after-tool`, …) are already mapped — reuse them if your plugin
can emit them, and you may not need to touch this function at all.

### 1.4 Wire correct provenance (strongly recommended)

The agent name flows through end-to-end as a free-form string
(`AITool::Cli(String)`), so recording will *work* without these. But several
hardcoded per-agent maps make the output correct and pretty. Update them:

| What | File:line | Why |
|------|-----------|-----|
| Vendor inference | `atomic-agent/src/record/provenance.rs:161` (`vendor_from_agent_name`) | Maps agent → `AIVendor`. Without an arm, vendor becomes `Other("<name>")`. |
| Author-is-agent classification | `atomic-repository/src/repository/provenance_summary.rs:208` (`KNOWN_AGENT_PREFIXES`) and the fallback name list (~`:241`) | Otherwise the agent's changes are miscounted as human-authored in `atomic agent attest --summary`. |
| Display prettifier | `atomic-cli/src/commands/agent/attest.rs:607` (`pretty_tool`) | Nice name in attestation output. |

Note: `AIVendor` itself is an enum (`atomic-core/src/change/provenance/types.rs:21`).
If your agent's provider isn't represented and you don't want `Other(...)`, add
a variant there and to its `parse()`.

### 1.5 (Optional) global install support

`atomic agent enable --global` uses a hardcoded `match agent_name` in
`atomic-cli/src/commands/agent/enable.rs` (~`:324`). Only add an arm if your
agent supports a single global settings file *and* you want `enable --global`
to be the installer. Plugin/manifest-style integrations don't need this — they
install themselves.

---

## Part 2 — The integration package `atomic-<agent>` (required)

This is a separate repo (published so users can install it). It contains no
Atomic Rust code — only the agent-facing assets and the wiring that makes the
agent call `atomic agent hooks …`.

Standard layout (see `atomic-opencode` and `atomic-claude`):

```
atomic-<agent>/
├── agents/<agent>.md   or  CLAUDE.md / AGENTS.md   # system prompt
├── skills/
│   ├── atomic-vault/SKILL.md
│   ├── atomic-vcs/SKILL.md
│   └── code-intelligence/SKILL.md
├── hooks/<agent>.atomic-hooks.json   # manifest  (native-hooks style)
│   —or—
├── plugins/atomic-hooks.ts           # plugin     (plugin style)
├── install.sh                        # dev install (symlinks)
├── install.js                        # npm postinstall
├── package.json
└── README.md
```

### 2.1 The system prompt

Ship the agent's system prompt in whatever file the agent reads. The prompt
location convention per agent is recorded in the harness
(`atomic-agents/.../env.rs`, `PromptKind`):

- `CLAUDE.md` at root — Claude Code
- `AGENTS.md` at root — Codex, Devin, Gemini
- `agents/*.md` — OpenCode, Pi
- `rules/*.md` — Cursor, Cline
- `steering/*.md` — Kiro
- `copilot-instructions.md` — Copilot

The prompt should teach the intent-first Atomic workflow (create an intent,
define the problem, plan, execute, and *don't* run `atomic add`/`record` —
hooks do that). Copy `atomic-claude/CLAUDE.md` or `atomic-opencode/agents/atomic.md`
as a starting point.

### 2.2 The skills

Skills are on-demand reference docs the agent loads when relevant. The common
set is `atomic-vault`, `atomic-vcs`, and `code-intelligence`. Reuse the
existing skill content; the installer symlinks them into the agent's skills
directory.

### 2.3 The lifecycle wiring — use the style the agent supports

Use whichever style the target agent's extension model allows (see the
constraints in [The three repositories](#the-three-repositories) above): a
manifest if it has a user-configurable hooks file, a plugin if it exposes a
plugin API, both-then-prefer-plugin if it supports both.

**A. Native-hooks style (manifest).** Write
`hooks/<agent>.atomic-hooks.json` describing where the agent's settings file is
and which commands to register. The manifest is merged idempotently by
`atomic agent enable --hooks <manifest>` — the merge engine is built into
`atomic` (no `jq`/Node needed), and it preserves the user's non-Atomic hooks.
See `atomic-claude/hooks/claude-code.atomic-hooks.json`. Format
(`atomic-agent/src/hooks/manifest.rs`):

```json
{
  "target": "~/.claude/settings.json",
  "hooks_key": "hooks",
  "command_prefix": "atomic agent hooks claude-code",
  "hooks": {
    "SessionStart": [ { "matcher": "", "hooks": [
      { "type": "command",
        "command": "test -d .atomic && atomic agent hooks claude-code session-start || true" } ] } ],
    "Stop":         [ { "matcher": "", "hooks": [
      { "type": "command",
        "command": "test -d .atomic && atomic agent hooks claude-code stop || true" } ] } ]
  },
  "merge": { "permissions": { "deny": [ "Read(./.atomic/metadata/**)" ] } }
}
```

- `target` — the agent's settings file (`~` expands to home).
- `command_prefix` — substring identifying *our* hooks so re-runs are
  idempotent and uninstall removes only ours.
- `hooks` — `event → [entry]` in the agent's native shape (nested
  `{matcher, hooks:[{command}]}` or flat `{command}` both supported).
- `merge` — extra non-hook settings deep-merged into the file.

The big win: because the manifest lives in the integration repo, changing hook
wiring never requires rebuilding `atomic` — just re-publish the package.

**B. Plugin style.** If the agent has a plugin API, write a plugin that
subscribes to lifecycle events and shells out to the CLI. See
`atomic-opencode/plugins/atomic-hooks.ts`. The essential shape:

```ts
// on each lifecycle event, pipe JSON to the CLI
await $`echo ${JSON.stringify(payload)} | atomic agent hooks <agent> ${verb}`.nothrow();
```

Map the agent's events to Atomic verbs, e.g. OpenCode:
`session.created→session-start`, `chat.message→user-prompt`,
`tool.execute.before→before-tool`, `tool.execute.after→after-tool`,
`session.idle→stop`, `session.deleted→session-end`. Send model, provider,
tokens, cost, and timing in the payload so provenance is rich.

Guard every hook so it only fires inside an Atomic repo:
`test -d .atomic && … || true` (or the sandbox-aware form in
`guarded_hook_command`, `atomic-agent/src/hooks/mod.rs:103`).

### 2.4 The installer

`install.sh` (dev) symlinks prompt + skills into the agent's config dir and, for
native-hooks style, calls `atomic agent enable --hooks <manifest>`. `install.js`
is the npm `postinstall` equivalent. Copy from whichever existing package
matches your style.

---

## Part 3 — Register the agent in the test harness `atomic-agents` (required)

The harness in `atomic-agents` auto-discovers and tests every agent that (a)
has an integration package present and (b) is spawnable per the live ACP
registry. To include your agent, add one entry to `AGENT_REGISTRY` in
`crates/atomic-agent-harness/src/env.rs` (~`:48`):

```rust
AgentEntry {
    registry_id: "myagent-acp",        // ID in the canonical ACP registry
    name: "myagent",                    // label in test output
    package: "atomic-myagent",          // dir name under AGENTS_DIR
    prompt: PromptKind::AgentsDir,      // where the prompt lives (see 2.1)
    installed_skills_dir: "~/.config/myagent/skills",
    skills: &["atomic-vault", "atomic-vcs", "code-intelligence"],
},
```

Notes:

- The **spawn command** is *not* hardcoded — it comes from the live ACP registry
  (`registry.rs`, fetched from `cdn.agentclientprotocol.com`). `registry_id`
  must match the agent's ID there. If the agent is stdio-only and needs a
  local install hint, add it to `spawn.rs`'s `AGENTS` table.
- `available_agents()` only returns an agent when its package directory exists
  in `AGENTS_DIR` (default `~/Projects/agents`) *and* the registry knows how to
  spawn it on this platform.

The harness then runs the shared `all_agents_*` integration tests against your
agent (`crates/atomic-agent-harness/tests/acp_integration.rs`): respond to a
prompt, use code search, check repo status, create an intent. These are
`#[ignore]` by default (they make real LLM calls); run explicitly.

---

## Checklist

**In `atomic` (this repo):**

- [ ] `atomic-agent/src/hooks/<agent>.rs` implements `AgentHook`.
- [ ] Registered in `AgentRegistry::with_defaults()` (`hooks/mod.rs`), module declared.
- [ ] Any new verbs added to `HookType::from_verb` (`event.rs`).
- [ ] Vendor arm in `vendor_from_agent_name` (`record/provenance.rs`).
- [ ] Author classification in `provenance_summary.rs` (`KNOWN_AGENT_PREFIXES` + fallback list).
- [ ] Display name in `pretty_tool` (`agent/attest.rs`).
- [ ] (Optional) `enable --global` arm (`agent/enable.rs`).
- [ ] `cargo test -p atomic-agent` passes; adapter has unit tests (copy `opencode.rs`'s test module).

**In `atomic-<agent>` (integration package):**

- [ ] System prompt in the agent's convention (`CLAUDE.md` / `agents/*.md` / …).
- [ ] Skills (`atomic-vault`, `atomic-vcs`, `code-intelligence`).
- [ ] Lifecycle wiring: a hooks manifest **or** a plugin that calls `atomic agent hooks <agent> <verb>`.
- [ ] Every hook guarded with `test -d .atomic && … || true`.
- [ ] `install.sh` / `install.js` + `package.json` + `README.md`.

**In `atomic-agents` (harness):**

- [ ] `AgentEntry` added to `AGENT_REGISTRY` (`env.rs`).
- [ ] `registry_id` matches the canonical ACP registry (add spawn hint to `spawn.rs` if stdio-only).

## Verifying end-to-end

```bash
# 1. Build atomic with your adapter.
cargo build -p atomic
cargo test -p atomic-agent

# 2. Install the integration package into your agent (dev mode).
cd ~/code/work/atomic-<agent> && ./install.sh

# 3. Smoke-test the hook path by hand — this is exactly what the agent will do.
cd /some/atomic/repo
echo '{"session_id":"t1","cwd":"'"$PWD"'"}' | atomic agent hooks <agent> session-start
echo '{"session_id":"t1","prompt":"hi","model":"...","provider":"..."}' | atomic agent hooks <agent> user-prompt
echo '{"session_id":"t1","turn_number":1}' | atomic agent hooks <agent> stop
atomic agent attest        # confirm a provenance/attestation record appeared

# 4. Full ACP integration test via the harness (real LLM calls; needs API key + package in AGENTS_DIR).
cd ~/code/work/atomic-agents
cargo test -p atomic-agent-harness --test acp_integration -- --ignored --nocapture
```

## What is agent-agnostic vs. what you must touch

Agent-agnostic (no change needed): the `atomic agent hooks <agent> <verb>`
command itself, the `AgentRegistry` dispatch machinery, the `AITool::Cli(String)`
provenance carrier, the manifest merge engine, and the W3C PROV projection
(the agent slug is free-form).

Must touch to add a *fully supported* agent: the adapter file, its registry
registration, and (if it uses new verb strings) the verb map — plus the
provenance/display maps for correct attribution.

## Reference: key files

- Trait + registry: `atomic-agent/src/hooks/mod.rs`
- Example adapter (plugin style): `atomic-agent/src/hooks/opencode.rs`
- Example adapter set: `atomic-agent/src/hooks/{claude_code,codex,gemini_cli,cursor,…}.rs`
- Verb → hook-type map: `atomic-agent/src/event.rs` (`HookType::from_verb`)
- Manifest install engine: `atomic-agent/src/hooks/manifest.rs`
- Hooks CLI entry: `atomic-cli/src/commands/agent/hooks.rs`
- Enable/attest CLI: `atomic-cli/src/commands/agent/{enable,attest}.rs`
- Provenance mapping: `atomic-agent/src/record/provenance.rs`,
  `atomic-repository/src/repository/provenance_summary.rs`
- Integration packages: `atomic-opencode/` (plugin), `atomic-claude/` (manifest)
- Test harness registry: `atomic-agents/crates/atomic-agent-harness/src/env.rs`
</content>
</invoke>
