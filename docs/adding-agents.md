# Adding a New AI Agent to Atomic

This document teaches you how to add support for a new AI coding agent
(Claude Code, OpenCode, Codex, Gemini CLI, Cursor, …) to the Atomic ecosystem.

It is written to be read top to bottom. The first half explains *what an agent
integration is* and *why it is built the way it is*. The second half walks you
through building one. By the end you should understand the system well enough
to add your own agent — and know exactly where to look for working examples.

---

## What "adding an agent" actually means

Every version control system records **what changed**: these lines were added,
those were removed, this file was renamed. Atomic does that too — but it also
records **how and why the change came to be**.

When an AI agent works in an Atomic repository, Atomic captures the full
provenance of the session alongside the code:

- **The prompt** — the human's actual request that kicked off the work.
- **The model and vendor** — which AI did the work (e.g. `claude-sonnet-4`,
  `gpt-5`), and which provider served it.
- **The cost** — input/output/reasoning token counts and USD spend per turn.
- **The decision graph** — a causal graph of the agent's tool calls: what it
  explored, what it edited, what it verified, what failed — and which step led
  to which.
- **The attestation** — a session-level audit record: this agent, this model,
  spent $X and N tokens producing these M changes.

This data is not logged to some external service. It is embedded into the
recorded changes themselves and stored content-addressed in the repository,
right next to the code. You can inspect it with `atomic change -p` (per-change
provenance) and `atomic agent attest` (session attestation). Months later,
"why is this line here?" has a real answer: the prompt that asked for it, the
model that wrote it, and the reasoning chain that produced it.

**That is what "adding an agent" means: teaching Atomic to capture that
provenance from one specific agent's lifecycle — and teaching that agent to
behave like an Atomic-native developer so the captured work is clean and
reviewable.**

An integration therefore has two halves:

| Half | Question it answers | Built from |
|------|--------------------|------------|
| **Behavior** | Does the agent work the Atomic way — intent-first, using Atomic's tools, not fighting the recorder? | A system prompt + three skills (shipped in the `atomic-<agent>` package) |
| **Capture** | Does Atomic receive the agent's lifecycle events and turn them into provenance-bearing changes? | Lifecycle wiring (plugin or hooks manifest) + a hook adapter in the `atomic` crate |

And one requirement that spans both: **testing**, via the agent harness.

### The three repositories

Agent support spans three repositories. Know which one owns which concern:

| Repo | Role | What lives here |
|------|------|-----------------|
| **`atomic`** (this repo) | The engine + CLI | The `atomic-agent` crate: per-agent *hook adapters*, the `AgentRegistry`, the verb→event map, the `TurnOrchestrator` that does the recording, and the `atomic agent …` CLI. This is where you write Rust so Atomic can *understand* your agent. |
| **`atomic-<agent>`** (e.g. [`atomic-opencode`](https://github.com/atomicdotdev/atomic-opencode), [`atomic-claude`](https://github.com/atomicdotdev/atomic-claude)) | The integration package | The agent-facing glue: the system prompt, the skills, the wiring that makes the agent call back into `atomic agent hooks …` (a plugin or a hooks manifest), and an installer. Published to npm / installed via `install.sh`. |
| **`atomic-agents`** ([repo](https://github.com/atomicdotdev/atomic-agents)) | The test harness | An ACP-based harness that spawns each agent, sends real prompts, and asserts on the ACP stream and repository side effects. This is where you register your agent so it gets *tested*. |

---

## The skills: what every agent carries

A **skill** is an on-demand reference document the agent loads when a task
calls for it (kept out of the always-on prompt to save context). Every Atomic
agent ships the same three core skills, so that **behavior is identical no
matter which agent the user runs** — the agent is interchangeable; the Atomic
workflow is not.

### 1. Intents (`atomic-vault`) — capturing the *why*

The vault is Atomic's built-in project-management and context layer. It tracks
**intents** (units of work, like tickets), **goals** (work sessions), and
**memory** (durable knowledge that persists across sessions).

This skill teaches the agent the intent lifecycle:

```bash
atomic vault intent list                          # always check first
atomic vault intent create --title "Implement JWT signing"
atomic vault intent update <ID> --status planned  # after the user accepts the plan
atomic vault sync                                 # persist .vault/ file edits to the DB
```

Why it matters: the prompt alone is not enough. The intent is where the agent
writes the **problem statement**, the **testable success criteria**, and the
**task list** — before writing any code. That artifact lives in the repo, so
the "why" survives the chat session. This is the foundation of Atomic's
problem-first workflow: the user's request is usually a *solution* ("build me
X"); the agent reframes it as a *problem*, gets agreement, then executes.

### 2. Code Intelligence (`code-intelligence`) — navigating the code

Atomic maintains a knowledge graph and content index of the repository. This
skill teaches the agent to use it **instead of blind `grep`/`find`**:

```bash
atomic vault query code "fn parse_query" -t rs   # source content search (grep replacement)
atomic vault query search "authentication"        # structural KG search: files, entities, changes
atomic vault query neighbors <node_id>            # relationships between entities
atomic vault query entities <path>                # file outline
```

Why it matters: exploration becomes structural and cheap. The agent finds the
function by relationship, not by scanning the filesystem — and Atomic learns
what the agent looked at, which feeds the decision graph.

### 3. VCS (`atomic-vcs`) — speaking the Atomic CLI

This skill teaches the agent the **read-only** side of the Atomic CLI:

```bash
atomic status          # what's different in the working copy right now?
atomic log             # recent history of this view
atomic change -p <id>  # what's in one change, including AI provenance
atomic change -a <id>  # AI attestation for a change
atomic diff            # precise line/token edits, working copy vs recorded
```

Why it matters: it lets the agent ground itself in what actually changed and
audit its own recorded work instead of guessing. It also reinforces the
division of labor: **reading history is the agent's job; recording is the
hook's job.**

> Some agents ship extras on top of the core three — `atomic-claude` adds
> `intent-builder` and `codebase-context` — but `atomic-vault`,
> `code-intelligence`, and `atomic-vcs` are the baseline every package
> includes. Reuse the existing `SKILL.md` files verbatim (copy them from
> `atomic-claude/skills/`); keeping the content shared is the point — fix a
> workflow detail once and every agent benefits.

---

## The system prompt: `AGENTS.md` and friends

The hooks and adapter make Atomic *record* what the agent does. The **system
prompt** makes the agent *do the right things* in the first place. It is the
most important file in the integration package.

Without it, you get perfect provenance for an agent that still thinks it's
using git, invents its own workflow, and greps the filesystem blindly. With
it, the agent works problem-first, keeps history clean, and uses Atomic's own
tools for discovery. **Capture without behavior is a faithful recording of a
mess.**

Each agent reads its system prompt from a different file convention:

| Convention | Agents |
|-----------|--------|
| `CLAUDE.md` at root | Claude Code |
| `AGENTS.md` at root | Codex, Devin, Gemini |
| `agents/*.md` | OpenCode, Pi |
| `rules/*.md` | Cursor, Cline |
| `steering/*.md` | Kiro |
| `copilot-instructions.md` | Copilot |

Whatever the filename, the prompt must establish the same four things. Real
examples to copy from: [`atomic-claude/CLAUDE.md`](https://github.com/atomicdotdev/atomic-claude/blob/main/CLAUDE.md)
(full version) and
[`atomic-opencode/agents/atomic.md`](https://github.com/atomicdotdev/atomic-opencode/blob/main/agents/atomic.md)
(compact version).

1. **"You use Atomic VCS, not git."** — Set the tool up front so the agent
   stops reaching for `git` commands.

2. **The intent-first workflow.** — Every unit of work becomes an *intent*:
   check for an existing one, create one if needed, reframe the request as a
   problem statement with testable success criteria, write the plan into the
   intent file, get human agreement, then execute and check off tasks. The
   plan lives in the file, not just in chat.

3. **"Do NOT run `atomic add` or `atomic record`."** — Critical. The hook
   system records automatically, with provenance, at turn end. If the agent
   records manually it produces changes with no AI provenance and can race the
   orchestrator. Companion rules: "do not create or switch views" (the session
   view is automatic) and "do not run `atomic agent enable`" (already
   configured).

4. **Point at the skills.** — Tell the agent to load `atomic-vault`,
   `atomic-vcs`, and `code-intelligence` on demand.

Copy an existing prompt and adapt the surface details (how that agent invokes
skills, its permission block, its frontmatter) rather than writing from
scratch — the workflow content should stay consistent across all agents.

---

## Hooks: how the agent reports to Atomic

A **hook** is a lifecycle event the agent fires, translated into a call to the
Atomic CLI. This is the capture half of the integration.

### The events we want

No matter which agent or wiring style, the whole job is to notify Atomic at
six moments in an agent's life. Each event is delivered as:

```bash
echo '<json payload>' | atomic agent hooks <agent> <verb>
```

Session state is persisted between calls under `.atomic/sessions/` — each hook
invocation is a fresh, stateless CLI process keyed by `session_id`.

| Event (`HookType`) | Typical verb(s) | Fires when | What Atomic does |
|--------------------|-----------------|------------|------------------|
| `SessionStart` | `session-start` | agent session opens | Forks a **draft view** for the session and switches the working copy onto it — 1 session = 1 view, so the agent's work is isolated and reviewable as a unit |
| `TurnStart` | `user-prompt`, `user-prompt-submit`, `before-agent` | user submits a prompt | Saves the prompt + model/provider, starts a file-change baseline, opens a **Goal** node in the decision graph |
| `PreToolUse` | `before-tool`, `pre-tool` | before a tool runs | Bookkeeping only (no output yet) |
| `PostToolUse` | `after-tool`, `post-tool` | after a tool runs | Appends a classified node (`Exploration` / `Commitment` / `Verification` / `Execution` / `Error`) plus inferred causal edges to the session's **decision graph** |
| `TurnEnd` | `stop`, `after-agent`, `turn-end` | agent finishes a turn | **Records a change** on the draft view with full AI provenance embedded |
| `SessionEnd` | `session-end` | agent session closes | Flushes any unrecorded work, writes the **AI attestation**, restores the parent view |

**The golden rule of responsibility:** the agent side (plugin or hook command)
only does two things — fire the event at the right moment, and attach a JSON
telemetry payload. **Atomic does all version-control work itself** inside the
`TurnOrchestrator` (`atomic-agent/src/turn/orchestrator.rs`). You never call
`atomic add` or `atomic record` from the integration; you emit events and
Atomic records. This is why a correct integration is small.

Two events deserve emphasis:

- **`TurnEnd` is the one that matters most.** This is where a turn's edits
  become a permanent, provenance-bearing change. The orchestrator checks the
  working copy (no changes → no empty record), auto-adds untracked files,
  records on the session's draft view, and embeds everything the payload
  carries: vendor, model, the prompt (hashed), token counts, `cost_usd`,
  finish reason, reasoning, the task plan — plus a session envelope. The more
  your payload sends, the richer `atomic change -p` output is. If you
  integrate only one event beyond `SessionStart`, make it this one.

- **Tool events are the "nice to have," not the backbone.** If your agent
  can't emit tool-level events, you simply get no decision graph — provenance
  still works (change, prompt, model, tokens, attestation are all captured at
  turn/session end).

Because Atomic does all the VCS work, the smallest useful integration is just
**two CLI calls**:

```bash
# Once, when the session starts (creates the draft view):
echo '{"session_id":"my-sess-1","cwd":"'"$PWD"'"}' \
  | atomic agent hooks <agent> session-start

# ... agent edits files ...

# When a unit of work finishes (records the change WITH provenance):
echo '{"session_id":"my-sess-1","prompt":"add retry logic",
       "model":"...","provider":"...","input_tokens":1200,"output_tokens":800,
       "cost_usd":0.04}' \
  | atomic agent hooks <agent> stop
```

Add `user-prompt` (captures the prompt/Goal), `before-tool`/`after-tool`
(builds the decision graph), and `session-end` (attestation + view restore) as
your agent is able to signal them. Every field beyond `session_id` is optional
and simply enriches the recorded provenance.

Every hook invocation should be guarded so it only fires inside an Atomic repo
and never breaks the agent if something fails:

```bash
test -d .atomic && atomic agent hooks <agent> <verb> || true
```

---

## Why each agent must be added explicitly

If hooks are just "pipe JSON to a CLI," why does every agent need its own Rust
adapter in this repo? **Because no two agents speak the same dialect.**

- **Different hook names.** Claude Code fires `Stop` when a turn ends and
  `UserPromptSubmit` when a prompt arrives. OpenCode fires `session.idle` and
  `chat.message`. Codex, Gemini, and Cursor each have their own names again.
- **Different payload shapes.** One agent sends
  `{"session_id": ..., "model": ...}`; another nests the model under
  `message.model.id`; a third doesn't send token counts at all. Each agent's
  JSON is its own schema.

Atomic's internals don't want to know about any of that. Internally there is
one **standardized event format**:

- `HookType` (`atomic-agent/src/event.rs:67`) — the six canonical events:
  `SessionStart`, `SessionEnd`, `TurnStart`, `TurnEnd`, `PreToolUse`,
  `PostToolUse`.
- `TurnEvent` (`atomic-agent/src/event.rs`) — one normalized struct carrying
  `session_id`, optional `prompt`, `tool_name`, `tool_use_id`, and a
  `raw_json` bag for everything else (model, provider, tokens, cost, …).

The **hook adapter** is the translator between the two. Each adapter:

1. Maps the agent's verb strings to a `HookType`
   (`HookType::from_verb`, `atomic-agent/src/event.rs:166` — one function that
   is the union of all agents' verb strings).
2. Deserializes that agent's stdin JSON and builds a `TurnEvent`
   (`AgentHook::parse_event`), stashing extra telemetry into `raw_json`.

```
agent-native event                    standardized event
─────────────────────                 ──────────────────
"session.idle" + OpenCode JSON  ──►   HookType::TurnEnd + TurnEvent
"Stop"         + Claude JSON    ──►   HookType::TurnEnd + TurnEvent
<your verb>    + your JSON      ──►   HookType::TurnEnd + TurnEvent
                                              │
                                              ▼
                                   TurnOrchestrator (one code path,
                                   agent-agnostic) → provenance-bearing change
```

That translation layer is the entire reason "adding an agent" is an explicit
act: you are teaching Atomic a new dialect so everything downstream —
recording, provenance, attestation, diffing — works through a single
agent-agnostic pipeline.

---

## Plugin vs. config file: choosing the wiring

The last design question is *how the agent's lifecycle events end up calling
the CLI*. There are two styles — and **which one you use is dictated by the
agent's extension model, not by preference.**

### A. Native-hooks manifest (the config-file style)

Used when the agent exposes its own hook system in a user-editable settings
file (Claude Code, Codex, Gemini, Cursor, …).

The integration package ships a **hooks manifest** JSON describing where the
agent's settings file lives and which commands to register
([`atomic-claude/hooks/claude-code.atomic-hooks.json`](https://github.com/atomicdotdev/atomic-claude/blob/main/hooks/claude-code.atomic-hooks.json)).
`atomic agent enable --hooks <manifest>` merges it into the agent's settings
file — the merge engine is built into `atomic`
(`atomic-agent/src/hooks/manifest.rs`), is idempotent, and preserves the
user's non-Atomic hooks:

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
- `command_prefix` — substring identifying *our* hooks, so re-runs are
  idempotent and uninstall removes only ours.
- `hooks` — event → entries in the agent's native shape.
- `merge` — extra non-hook settings deep-merged into the file.

The big win: the manifest is **declarative and lives in the integration
package**, so changing hook wiring never requires rebuilding `atomic` — just
re-publish the package.

### B. Plugin (the code style)

Used when the agent exposes a plugin/extension API instead of a hooks config
(OpenCode, …).

The integration package ships a plugin that subscribes to lifecycle events in
code and shells out to the CLI
([`atomic-opencode/plugins/atomic-hooks.ts`](https://github.com/atomicdotdev/atomic-opencode/blob/main/plugins/atomic-hooks.ts)).
The essential shape:

```ts
// On each lifecycle event, pipe JSON to the CLI:
await $`echo ${JSON.stringify(payload)} | atomic agent hooks <agent> ${verb} 2>/dev/null`.nothrow();
```

The plugin maps the agent's events to Atomic verbs — OpenCode's mapping is
`session.created→session-start`, `chat.message→user-prompt`,
`tool.execute.before→before-tool`, `tool.execute.after→after-tool`,
`session.idle→after-agent` (→ `TurnEnd`), `session.deleted→session-end`.

A plugin is more work than a manifest, but it earns its keep: it can observe
**rich telemetry a shell hook cannot** — model, provider, token counts, cost,
timing — and put it in the payload, which is exactly what makes recorded
provenance valuable. The plugin also decides *when* to fire (e.g. OpenCode's
plugin auto-records on idle as a safety net) and disables itself outside
Atomic repos.

### How to choose

| Agent exposes… | You ship… |
|----------------|-----------|
| Only a plugin API (no user hooks config) | A plugin — your only option (this is why `atomic-opencode` exists) |
| Only a settings/hooks file (no plugin API) | A hooks manifest — your only option (this is why `atomic-claude` exists) |
| Both | Prefer whichever gives richer lifecycle data (usually the plugin: model/tokens/cost/timing) with the simpler install |
| Neither cleanly | Degraded mode: use whatever coarse signal exists (e.g. session boundary) and have the adapter's `supported_hooks()` advertise only what you can actually observe |

**Investigate the target agent's extension model first and let it decide.**
Whichever style you land on, the payload that reaches Atomic is identical:
JSON on stdin to `atomic agent hooks <agent> <verb>`.

### Worked example: the OpenCode flow end-to-end

```
user runs `opencode` in a project that has .atomic/
        │
        ▼
OpenCode loads plugins/atomic-hooks.ts (registered in opencode.json)
        │  the plugin first checks: `atomic` on PATH and .atomic/ present?
        │  if not, it disables itself. No atomic repo → no-op.
        ▼
OpenCode fires lifecycle events; the plugin translates each to a CLI call:
        session.created     → atomic agent hooks opencode session-start
        chat.message        → atomic agent hooks opencode user-prompt
        tool.execute.after  → atomic agent hooks opencode after-tool
        session.idle        → atomic agent hooks opencode after-agent
        session.deleted     → atomic agent hooks opencode session-end
        │  each call pipes a JSON payload (model, provider, tokens, timing)
        ▼
`atomic agent hooks opencode <verb>`  (atomic-cli/src/commands/agent/hooks.rs)
        │  AgentRegistry.require("opencode") → OpenCodeHook
        │  OpenCodeHook::parse_event(verb → HookType, stdin JSON) → TurnEvent
        ▼
TurnOrchestrator records the turn as an Atomic change with AI provenance
(1 session = 1 draft view)
```

The plugin is the driver (it decides when to call Atomic and what to send);
the Rust adapter is a thin parser that turns the payload into a `TurnEvent`.
The user never types `atomic agent hooks …` — the plugin does, automatically,
on every event.

---

## Building the integration

Now the concrete work, in three parts. Everything above is the theory; this is
the generalization of the flow you just saw.

### Part 1 — The hook adapter in `atomic` (required)

Everything in the `atomic agent hooks <agent> <verb>` pipeline is keyed off a
string agent name, dispatched through the `AgentRegistry`. To make Atomic
understand a new agent you implement one trait and register it.

#### 1.1 Create the adapter file

Create `atomic-agent/src/hooks/<agent>.rs`. Use
[`atomic-agent/src/hooks/opencode.rs`](atomic-agent/src/hooks/opencode.rs) as
the reference implementation — it is the cleanest example and covers all six
hook types.

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

- Deserialize the agent's stdin JSON (each agent sends a different shape —
  model one struct per verb, as `opencode.rs` does).
- Build a `TurnEvent` with `session_id`, attaching `prompt`, `tool_name`,
  `tool_use_id` where relevant.
- Stash anything else the orchestrator/provenance wants (model, provider,
  tokens, cost, finish reason) into `raw_json` — see how `opencode.rs`
  re-inserts `model`/`provider` into the raw JSON object.

An agent need not support all six hook types — return only what it can
actually emit from `supported_hooks()` and `hook_verbs()`.

Regarding `install`/`uninstall`/`is_installed`: if the integration package
handles installation itself (plugin or manifest style), these are near no-ops.
OpenCode's adapter just checks whether the plugin file exists, because the
`atomic-opencode` npm package owns installation. Only implement real
config-file writing here if you want `atomic agent enable --agent <name>` to
be the installer.

#### 1.2 Register the adapter

Add the module and register it in the default registry
(`atomic-agent/src/hooks/mod.rs`):

```rust
// with the other `pub mod` declarations (~line 59)
pub mod myagent;

// in AgentRegistry::with_defaults() (~line 319)
registry.register(Box::new(myagent::MyAgentHook::new()));
```

That is the *only* place that decides which agents exist. `atomic agent
hooks`, `enable`, `status`, and auto-detection all read from this registry.

#### 1.3 Map the agent's verbs to hook types

`atomic agent hooks <agent> <verb>` turns `<verb>` into a `HookType` via
`HookType::from_verb` (`atomic-agent/src/event.rs:166`). This one function is
the union of *all* agents' verb strings. If your agent introduces a verb
string that is not already mapped, add an arm:

```rust
"my-turn-begin" => Some(HookType::TurnStart),
"my-turn-done"  => Some(HookType::TurnEnd),
```

Many common verbs (`session-start`, `session-end`, `user-prompt`, `stop`,
`before-tool`, `after-tool`, …) are already mapped — reuse them if your plugin
can emit them, and you may not need to touch this function at all.

#### 1.4 Wire correct provenance (strongly recommended)

The agent name flows through end-to-end as a free-form string
(`AITool::Cli(String)`), so recording will *work* without these. But several
hardcoded per-agent maps make the output correct and pretty:

| What | Where | Why |
|------|-------|-----|
| Vendor inference | `atomic-agent/src/record/provenance.rs:161` (`vendor_from_agent_name`) | Maps agent → `AIVendor`. Without an arm, vendor becomes `Other("<name>")`. |
| Author-is-agent classification | `atomic-repository/src/repository/provenance_summary.rs:208` (`KNOWN_AGENT_PREFIXES`) + fallback list (~`:241`) | Otherwise the agent's changes are miscounted as human-authored in `atomic agent attest --summary`. |
| Display prettifier | `atomic-cli/src/commands/agent/attest.rs:607` (`pretty_tool`) | Nice name in attestation output. |

`AIVendor` itself is an enum (`atomic-core/src/change/provenance/types.rs:21`);
if your agent's provider isn't represented and you don't want `Other(...)`,
add a variant there and to its `parse()`.

#### 1.5 (Optional) global install support

`atomic agent enable --global` uses a hardcoded `match agent_name` in
`atomic-cli/src/commands/agent/enable.rs` (~`:324`). Only add an arm if your
agent supports a single global settings file *and* you want `enable --global`
to be the installer. Plugin/manifest-style integrations don't need this —
they install themselves.

### Part 2 — The integration package `atomic-<agent>` (required)

A separate repo (published so users can install it). It contains no Atomic
Rust code — only the agent-facing assets and the wiring.

Standard layout (see [`atomic-opencode`](https://github.com/atomicdotdev/atomic-opencode)
and [`atomic-claude`](https://github.com/atomicdotdev/atomic-claude)):

```
atomic-<agent>/
├── agents/<agent>.md   or  CLAUDE.md / AGENTS.md   # system prompt (see above)
├── skills/
│   ├── atomic-vault/SKILL.md
│   ├── atomic-vcs/SKILL.md
│   └── code-intelligence/SKILL.md
├── hooks/<agent>.atomic-hooks.json   # manifest  (config-file style)
│   —or—
├── plugins/atomic-hooks.ts           # plugin    (plugin style)
├── install.sh                        # dev install (symlinks into the agent's config dir)
├── install.js                        # npm postinstall equivalent
├── package.json
└── README.md
```

You have already seen the three ingredients — the prompt, the skills, and the
wiring — in the conceptual half of this doc. Assembly notes:

- **Prompt**: ship it under the filename convention your agent reads (see the
  table in [The system prompt](#the-system-prompt-agentsmd-and-friends)).
- **Skills**: copy the three core `SKILL.md` files verbatim; the installer
  symlinks them into the agent's skills directory (e.g.
  `~/.config/opencode/skills/`, `~/.claude/skills/`).
- **Wiring**: whichever style the agent's extension model dictates (see
  [Plugin vs. config file](#plugin-vs-config-file-choosing-the-wiring)).
  Guard every hook: `test -d .atomic && … || true`.
- **Installer**: `install.sh` (dev) symlinks the prompt + skills and, for
  manifest style, calls `atomic agent enable --hooks <manifest>`. `install.js`
  is the npm `postinstall` equivalent. Copy from whichever existing package
  matches your style.

### Part 3 — Register the agent in the test harness (required)

The harness in `atomic-agents` auto-discovers and tests every agent that (a)
has an integration package present and (b) is spawnable per the live ACP
registry. To include yours, add one entry to `AGENT_REGISTRY` in
`crates/atomic-agent-harness/src/env.rs` (~`:48`):

```rust
AgentEntry {
    registry_id: "myagent-acp",        // ID in the canonical ACP registry
    name: "myagent",                    // label in test output
    package: "atomic-myagent",          // dir name under AGENTS_DIR
    prompt: PromptKind::AgentsDir,      // where the prompt lives
    installed_skills_dir: "~/.config/myagent/skills",
    skills: &["atomic-vault", "atomic-vcs", "code-intelligence"],
}
```

Notes:

- The **spawn command** is *not* hardcoded — it comes from the live ACP
  registry (`registry.rs`, fetched from `cdn.agentclientprotocol.com`).
  `registry_id` must match the agent's ID there. If the agent is stdio-only
  and needs a local install hint, add it to `spawn.rs`'s `AGENTS` table.
- `available_agents()` only returns an agent when its package directory exists
  in `AGENTS_DIR` (default `~/Projects/agents`) *and* the registry knows how
  to spawn it on this platform.

---

## Testing: use the agent harness

**Do not rely on manual smoke tests alone — register your agent in the
[harness](https://github.com/atomicdotdev/atomic-agents) and run it.** The
harness exists precisely because integrations fail in ways unit tests can't
see: it spawns the *real agent* over ACP, sends it *real prompts*, and asserts
on both the ACP stream and the repository side effects. It verifies your
prompt and skills actually work — that the agent responds, uses code search,
checks repo status, and creates intents the way the workflow demands.

The shared `all_agents_*` integration tests live in
`crates/atomic-agent-harness/tests/acp_integration.rs`. They are `#[ignore]`d
by default (they make real LLM calls); run them explicitly:

```bash
# 1. Build atomic with your adapter and run its unit tests.
cargo build -p atomic
cargo test -p atomic-agent          # adapter unit tests — copy opencode.rs's test module

# 2. Install the integration package (dev mode: symlinks, edits go live immediately).
cd ~/code/work/atomic-<agent> && ./install.sh

# 3. Manual smoke test — this is exactly what the agent will do.
cd /some/atomic/repo
echo '{"session_id":"t1","cwd":"'"$PWD"'"}' | atomic agent hooks <agent> session-start
echo '{"session_id":"t1","prompt":"hi","model":"...","provider":"..."}' | atomic agent hooks <agent> user-prompt
echo '{"session_id":"t1","turn_number":1}' | atomic agent hooks <agent> stop
atomic agent attest                 # confirm a provenance/attestation record appeared

# 4. Full ACP integration test via the harness (needs API key + package in AGENTS_DIR).
cd ~/code/work/atomic-agents
cargo test -p atomic-agent-harness --test acp_integration -- --ignored --nocapture
```

Step 3 validates the capture half (events → provenance). Step 4 validates the
behavior half (prompt + skills → correct agent behavior). You want both green
before calling the integration done.

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

- [ ] System prompt in the agent's convention (`CLAUDE.md` / `AGENTS.md` / `agents/*.md` / …).
- [ ] Skills: `atomic-vault`, `atomic-vcs`, `code-intelligence`.
- [ ] Lifecycle wiring: a hooks manifest **or** a plugin that calls `atomic agent hooks <agent> <verb>`.
- [ ] Every hook guarded with `test -d .atomic && … || true`.
- [ ] `install.sh` / `install.js` + `package.json` + `README.md`.

**In `atomic-agents` (harness):**

- [ ] `AgentEntry` added to `AGENT_REGISTRY` (`env.rs`).
- [ ] `registry_id` matches the canonical ACP registry (add spawn hint to `spawn.rs` if stdio-only).
- [ ] `acp_integration` tests pass for your agent.

## Example repositories

| Repo | Style | Look at it for |
|------|-------|----------------|
| [`atomicdotdev/atomic-opencode`](https://github.com/atomicdotdev/atomic-opencode) | Plugin | `plugins/atomic-hooks.ts` (lifecycle → CLI), `agents/atomic.md` (compact prompt) |
| [`atomicdotdev/atomic-claude`](https://github.com/atomicdotdev/atomic-claude) | Hooks manifest | `hooks/claude-code.atomic-hooks.json` (manifest), `CLAUDE.md` (full prompt), all five skills |
| [`atomicdotdev/atomic-agents`](https://github.com/atomicdotdev/atomic-agents) | Test harness | `crates/atomic-agent-harness/src/env.rs` (agent registry) |
| [`atomicdotdev/atomic`](https://github.com/atomicdotdev/atomic) | Engine (this repo) | `atomic-agent/src/hooks/opencode.rs` (reference adapter) |

## Reference: key files

- Trait + registry: `atomic-agent/src/hooks/mod.rs`
- Example adapter (plugin style): `atomic-agent/src/hooks/opencode.rs`
- More adapters: `atomic-agent/src/hooks/{claude_code,codex,gemini_cli,cursor,…}.rs`
- Verb → hook-type map: `atomic-agent/src/event.rs` (`HookType::from_verb`)
- Manifest install engine: `atomic-agent/src/hooks/manifest.rs`
- Orchestrator (does the recording): `atomic-agent/src/turn/orchestrator.rs`
- Hooks CLI entry: `atomic-cli/src/commands/agent/hooks.rs`
- Enable/attest CLI: `atomic-cli/src/commands/agent/{enable,attest}.rs`
- Provenance mapping: `atomic-agent/src/record/provenance.rs`,
  `atomic-repository/src/repository/provenance_summary.rs`
- Test harness registry: `atomic-agents/crates/atomic-agent-harness/src/env.rs`
