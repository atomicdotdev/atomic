# opencode-atomic-hooks

Atomic VCS provenance tracking plugin for [OpenCode](https://opencode.ai) — turns every AI agent turn into a content-addressed change with full causal decision graphs.

Translates OpenCode lifecycle events into `atomic agent hooks opencode <verb>` CLI calls — the same pattern used by Claude Code and Gemini CLI.

## Install

Add the plugin to your `opencode.jsonc`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["opencode-atomic-hooks"]
}
```

### Prerequisites

- [Atomic VCS](https://github.com/atomicdotdev/atomic) CLI installed and in your PATH
- An initialized Atomic repository (`.atomic/` directory exists)
- [OpenCode](https://opencode.ai) with plugin support enabled

### Activate

No additional configuration beyond the plugin entry. When OpenCode starts, the plugin checks for an Atomic repository (`.atomic/` directory) and activates automatically:

```
[atomic-hooks] [info] Atomic hooks plugin activated
```

If the project is not an Atomic repository or the `atomic` CLI is not in PATH, the plugin silently deactivates — zero overhead.

### Disable

Remove the plugin entry from `opencode.jsonc`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": []
}
```

## How It Works

```
User prompts AI in OpenCode
       │
       ▼
OpenCode fires lifecycle events
       │
       ▼
opencode-atomic-hooks plugin         ← loaded from npm
       │
       ├── handlers/event.ts   → session-start, stop, session-end
       ├── handlers/chat.ts    → user-prompt
       ├── handlers/tool.ts    → before-tool, after-tool
       ├── handlers/shell.ts   → ATOMIC_AGENT=opencode
       └── handlers/compaction.ts → provenance graph summary
       │
       ▼
atomic agent hooks opencode <verb>   ← JSON piped to stdin
       │
       ▼
atomic-agent/src/hooks/opencode.rs   ← parse_event()
       │
       ▼
TurnOrchestrator::dispatch()         ← state machine → record change
                                       + provenance graph accumulation
```

Each completed turn becomes a single Atomic change with full provenance — model, provider, tokens, cost, session ID, and turn number — all embedded in the change itself.

## Event Mapping

| OpenCode Hook             | Atomic Verb     | Rust HookType | Handler Module          |
|---------------------------|-----------------|---------------|-------------------------|
| `event` (session.created) | `session-start` | SessionStart  | `handlers/event.ts`     |
| `chat.message`            | `user-prompt`   | TurnStart     | `handlers/chat.ts`      |
| `event` (session.idle)    | `stop`          | TurnEnd       | `handlers/event.ts`     |
| `event` (session.deleted) | `session-end`   | SessionEnd    | `handlers/event.ts`     |
| `tool.execute.before`     | `before-tool`   | PreToolUse    | `handlers/tool.ts`      |
| `tool.execute.after`      | `after-tool`    | PostToolUse   | `handlers/tool.ts`      |
| `shell.env`               | *(env vars)*    | —             | `handlers/shell.ts`     |
| `experimental.session.compacting` | *(graph summary)* | — | `handlers/compaction.ts` |

## Module Architecture

```
src/
├── index.ts              ← Plugin entry point + barrel exports
├── types.ts              ← TypeScript interfaces & type aliases
├── constants.ts          ← Configuration constants & defaults
├── cli.ts                ← Atomic CLI invocation layer
├── session.ts            ← In-memory session state management
├── log.ts                ← Structured logging wrapper
├── handlers/
│   ├── index.ts          ← Handler barrel exports
│   ├── event.ts          ← Event bus → session lifecycle hooks
│   ├── chat.ts           ← Chat message → TurnStart hook
│   ├── tool.ts           ← Tool execution → PreToolUse/PostToolUse
│   ├── shell.ts          ← Shell env variable injection
│   └── compaction.ts     ← Provenance graph → compaction context
└── __tests__/
    ├── session.test.ts   ← Session store unit tests
    ├── handlers.test.ts  ← Handler helper unit tests
    └── compaction.test.ts ← Compaction handler unit tests
```

### Module Responsibilities

| Module                     | Purpose                                                                      | Key Exports                                                                      |
|----------------------------|------------------------------------------------------------------------------|----------------------------------------------------------------------------------|
| **index.ts**               | Plugin entry point — gates on Atomic repo, wires deps, returns hooks object  | `AtomicHooksPlugin`, plus all barrel re-exports                                  |
| **types.ts**               | All shared TypeScript interfaces — hook payloads, session state, CLI results | `HookVerb`, `SessionState`, `HookResult`, `AtomicHooksConfig`, payload types     |
| **constants.ts**           | Compile-time configuration — CLI binary name, hook args, edit tool set       | `ATOMIC_CMD`, `HOOK_ARGS`, `EDIT_TOOLS`, `MAX_TOOL_OUTPUT_LENGTH`                |
| **cli.ts**                 | Encapsulates all Atomic CLI interaction — pipes JSON to stdin                | `invokeHook()`, `isAtomicAvailable()`, `basePayload()`                           |
| **session.ts**             | In-memory session state — turn counts, model/provider, tool durations        | `createSessionStore()`, `SessionStore` interface                                 |
| **log.ts**                 | Structured logging via OpenCode SDK `client.app.log()` with console fallback | `createLogger()`, `Logger` interface                                             |
| **handlers/event.ts**      | Routes event bus events to session lifecycle hooks                            | `createEventHandler()`                                                           |
| **handlers/chat.ts**       | Captures prompt text and model info on user messages                         | `createChatHandler()`, `extractPrompt()`                                         |
| **handlers/tool.ts**       | Tracks tool timing, classifies file mutations, truncates output              | `createBeforeToolHandler()`, `createAfterToolHandler()`, `isEditTool()`          |
| **handlers/shell.ts**      | Injects `ATOMIC_AGENT` and `ATOMIC_AGENT_VERSION` into shell env             | `createShellHandler()`                                                           |
| **handlers/compaction.ts** | Reads provenance graph from disk, injects summary into compaction context    | `createCompactionHandler()`                                                      |

### Dependency Graph

No circular dependencies. Each module imports only from modules "below" it:

```
index.ts           → cli, session, log, handlers/*
handlers/*         → cli, session, log, constants, types
handlers/compaction → log (reads graph.json from disk, no CLI calls)
cli                → constants, types
session            → types
log                → constants, types
constants          → types
types              → (nothing)
```

## Design Principles

### 1. Best-Effort Recording

CLI failures are **logged, never thrown**. The AI session must never be blocked by a provenance recording failure. Every `invokeHook()` call returns a `HookResult` with timing and error details — but never throws.

### 2. Dependency Injection

Every handler is a factory function that accepts a `Deps` object:

```typescript
const deps = { $, store, log, directory }

return {
  event: createEventHandler(deps),
  "chat.message": createChatHandler(deps),
  "tool.execute.before": createBeforeToolHandler(deps),
  "tool.execute.after": createAfterToolHandler(deps),
  "shell.env": createShellHandler(),
}
```

This makes every handler fully testable — pass in mocks for the shell and logger without touching the filesystem or PATH.

### 3. Separation of Concerns

- **types.ts** — What data looks like (zero runtime code)
- **constants.ts** — What values are (zero logic)
- **cli.ts** — How we talk to the Atomic binary
- **session.ts** — What we remember between events
- **handlers/** — How we react to each event
- **log.ts** — How we report what happened

### 4. Ephemeral State

The `SessionStore` is in-memory only. If OpenCode restarts, all session state is lost. This is intentional — the Atomic CLI maintains its own durable session state in `.atomic/sessions/`. The plugin's state is just a correlation buffer to enrich hook payloads with timing and context.

## Usage

Once installed, the plugin activates automatically when OpenCode starts in an Atomic repository. Every turn is recorded:

```bash
# See what was recorded
atomic log

# Check session status
atomic agent status --verbose

# Rewind to a previous turn
atomic unrecord
```

## Testing

Run the unit test suite with Bun:

```bash
bun test

# Watch mode for development
bun test --watch

# Run a specific test file
bun test src/__tests__/session.test.ts
bun test src/__tests__/handlers.test.ts
bun test src/__tests__/compaction.test.ts
```

### Test Coverage

| Test File             | Module                  | Tests | Covers                                                                   |
|-----------------------|-------------------------|-------|--------------------------------------------------------------------------|
| `session.test.ts`     | `session.ts`            | ~40   | Store CRUD, turn counting, model/provider, tool duration, edge cases     |
| `handlers.test.ts`    | `handlers/*`            | ~60   | `extractPrompt`, `isEditTool`, `truncateOutput`, `createShellHandler`    |
| `compaction.test.ts`  | `handlers/compaction.ts`| ~22   | Graph reading, summary formatting, missing/corrupt files, token budget   |

### Adding Tests

All tests live in `src/__tests__/` and use [Bun's built-in test runner](https://bun.sh/docs/cli/test):

```typescript
import { describe, test, expect, beforeEach } from "bun:test"
import { createSessionStore } from "../session"

describe("myFeature", () => {
  test("does the thing", () => {
    const store = createSessionStore()
    store.incrementTurn("s1")
    expect(store.get("s1").turnCount).toBe(1)
  })
})
```

## Configuration

All configuration is currently compile-time constants in `constants.ts`. Environment variable overrides:

| Variable       | Default  | Purpose                              |
|----------------|----------|--------------------------------------|
| `ATOMIC_CMD`   | `atomic` | Atomic CLI binary name or path       |
| `ATOMIC_DEBUG` | `0`      | Set to `1` to enable debug logging   |

## Comparison with Other Agents

This plugin follows the exact same hook pattern as Claude Code and Gemini CLI:

| Aspect          | Claude Code                      | Gemini CLI                       | OpenCode                              |
|-----------------|----------------------------------|----------------------------------|---------------------------------------|
| Hook system     | Native `.claude/settings.json`   | Native `.gemini/settings.json`   | `opencode-atomic-hooks` npm package   |
| Installation    | Modify JSON config               | Modify JSON config               | Add to `opencode.jsonc` plugin array  |
| Invocation      | Agent calls CLI directly         | Agent calls CLI directly         | Plugin calls CLI                      |
| Turn boundary   | `stop` event                     | `AfterAgent` event               | `session.idle` event                  |
| Prompt capture  | `UserPromptSubmit`               | `BeforeAgent`                    | `chat.message` hook                   |
| Rust adapter    | `hooks/claude_code.rs`           | `hooks/gemini_cli.rs`            | `hooks/opencode.rs`                   |

OpenCode uses a **plugin package** approach rather than modifying a settings JSON file. This means zero modifications to the OpenCode codebase — the plugin is a self-contained TypeScript module published to npm that uses the public plugin API.

## Package Structure

```
opencode-atomic-hooks/
├── package.json          ← npm metadata, peer dep on @opencode-ai/plugin
├── tsconfig.json         ← declaration output for types
├── README.md
├── src/
│   ├── index.ts          ← plugin entry + barrel exports
│   ├── types.ts          ← TypeScript interfaces
│   ├── constants.ts      ← configuration constants
│   ├── cli.ts            ← Atomic CLI invocation
│   ├── session.ts        ← in-memory session state
│   ├── log.ts            ← structured logging
│   ├── handlers/
│   │   ├── index.ts      ← handler barrel exports
│   │   ├── event.ts      ← session lifecycle
│   │   ├── chat.ts       ← user prompt capture
│   │   ├── tool.ts       ← tool before/after
│   │   ├── shell.ts      ← env injection
│   │   └── compaction.ts ← provenance graph → compaction context
│   └── __tests__/
│       ├── session.test.ts
│       ├── handlers.test.ts
│       └── compaction.test.ts
└── dist/                 ← build output (bun build + tsc)
    ├── index.js          ← 16.5KB single bundle
    └── *.d.ts            ← type declarations
```

### Build

```bash
bun run build
```

Produces a single ESM bundle at `dist/index.js` via `bun build` (all 11 modules
inlined) plus TypeScript declaration files via `tsc --emitDeclarationOnly`. The
bundle targets Bun's runtime since OpenCode uses Bun.

### Publish

```bash
bun run build
npm publish
```

Users add it to their `opencode.jsonc`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["opencode-atomic-hooks"]
}
```

OpenCode's plugin loader installs it via `bun install`, imports the default
export, and calls the plugin function with the standard `PluginInput` context.
Same path as every plugin on the
[awesome-opencode](https://github.com/awesome-opencode/awesome-opencode) list.

### Local Development

For development within the opencode monorepo, a shim at
`.opencode/plugins/atomic.ts` re-exports from the workspace package:

```typescript
export { AtomicHooksPlugin } from "opencode-atomic-hooks"
```

This file matches the plugin discovery glob (`plugins/*.ts`) so OpenCode loads
it during local development without publishing to npm.

## Provenance Graph

This plugin includes a **compaction handler** that reads the provenance graph
built by the Rust-side `TurnOrchestrator` and injects a structured summary
into OpenCode's compaction context. This means the LLM retains knowledge of
what was explored, decided, committed, and verified — even after the
conversation is compacted to fit the context window.

The provenance graph itself is built and stored entirely on the Rust side.
The plugin only reads it from `.atomic/sessions/{id}/graph.json` during
compaction — no graph accumulation, no classification, no state management
in TypeScript.

## License

Dual-licensed under MIT and Apache 2.0, consistent with the Atomic VCS project.