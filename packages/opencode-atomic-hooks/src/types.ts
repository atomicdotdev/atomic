/**
 * Atomic VCS Hooks Plugin — Type Definitions
 *
 * Shared types used across all plugin modules. These mirror the JSON
 * payloads that flow between the OpenCode plugin and the Atomic CLI
 * (`atomic agent hooks opencode <verb>`).
 *
 * @module atomic/types
 */

import type { Plugin } from "@opencode-ai/plugin";

// =============================================================================
// Plugin Context
// =============================================================================

/**
 * The context object provided to the plugin by OpenCode at initialization.
 *
 * This is the parameter to the plugin factory function. We extract the
 * pieces we need so handlers don't depend on the full Plugin type.
 */
export type PluginContext = Parameters<Plugin>[0];

/**
 * The Bun shell (`$`) from the plugin context.
 *
 * Used exclusively by the CLI invocation layer to spawn
 * `atomic agent hooks opencode <verb>` processes.
 */
export type Shell = PluginContext["$"];

/**
 * The OpenCode SDK client from the plugin context.
 *
 * Used for structured logging via `client.app.log()`.
 */
export type Client = PluginContext["client"];

// =============================================================================
// Hook Verbs
// =============================================================================

/**
 * The set of hook verbs recognized by `atomic agent hooks opencode`.
 *
 * Each verb maps to an Atomic `HookType` on the Rust side:
 *
 * | Verb            | HookType       | Trigger                    |
 * |-----------------|----------------|----------------------------|
 * | `session-start` | SessionStart   | `session.created` event    |
 * | `session-end`   | SessionEnd     | `session.deleted` event    |
 * | `user-prompt`   | TurnStart      | `chat.message` hook        |
 * | `stop`          | TurnEnd        | `session.idle` event       |
 * | `before-tool`   | PreToolUse     | `tool.execute.before` hook |
 * | `after-tool`    | PostToolUse    | `tool.execute.after` hook  |
 */
export type HookVerb =
  | "session-start"
  | "session-end"
  | "user-prompt"
  | "stop"
  | "before-tool"
  | "after-tool";

// =============================================================================
// Hook Payloads — sent as JSON to stdin of `atomic agent hooks opencode <verb>`
// =============================================================================

/** Base fields present in every hook payload. */
export interface BasePayload {
  /** OpenCode session ID */
  session_id: string;
  /** Project working directory */
  cwd: string;
  /** ISO 8601 timestamp of the event */
  timestamp: string;
}

/** Payload for `session-start` verb. */
export interface SessionStartPayload extends BasePayload {
  /** How the session started: "startup" | "resume" */
  source: string;
  /** Model identifier, if known at session creation */
  model?: string;
  /** Provider identifier, if known at session creation */
  provider?: string;
}

/** Payload for `session-end` verb. */
export interface SessionEndPayload extends BasePayload {
  /** Why the session ended: "deleted" | "exit" | "error" */
  reason: string;
}

/** Payload for `user-prompt` verb (TurnStart). */
export interface UserPromptPayload extends BasePayload {
  /** The user's prompt text */
  prompt?: string;
  /** Model identifier (e.g., "claude-sonnet-4-20250514") */
  model?: string;
  /** Provider identifier (e.g., "anthropic", "openai") */
  provider?: string;
}

/** Payload for `stop` verb (TurnEnd). */
export interface StopPayload extends BasePayload {
  /** Which turn number this is (1-based) */
  turn_number: number;
  /** Model identifier */
  model?: string;
  /** Provider identifier */
  provider?: string;
  /** Whether the turn ended due to an error */
  error?: boolean;
}

/** Payload for `before-tool` verb (PreToolUse). */
export interface BeforeToolPayload extends BasePayload {
  /** Tool name (e.g., "edit", "bash", "read") */
  tool_name: string;
  /** Unique identifier for this tool invocation */
  tool_call_id: string;
  /** Arguments passed to the tool */
  tool_input: unknown;
}

/** Payload for `after-tool` verb (PostToolUse). */
export interface AfterToolPayload extends BasePayload {
  /** Tool name */
  tool_name: string;
  /** Unique identifier for this tool invocation */
  tool_call_id: string;
  /** Execution status: "completed" | "error" */
  status: "completed" | "error";
  /** Execution duration in milliseconds */
  duration?: number;
  /** Whether the tool modified files on disk */
  modified_files: boolean;
  /** Truncated tool output (max 500 chars) */
  tool_output?: string;
}

/**
 * Union of all hook payloads, discriminated by verb.
 *
 * Useful for generic hook invocation functions that accept any payload.
 */
export type HookPayload =
  | SessionStartPayload
  | SessionEndPayload
  | UserPromptPayload
  | StopPayload
  | BeforeToolPayload
  | AfterToolPayload;

// =============================================================================
// Session State
// =============================================================================

/**
 * In-memory state tracked for each active OpenCode session.
 *
 * This is **not** persisted — it lives only for the duration of the
 * OpenCode process. The Atomic CLI maintains its own durable session
 * state in `.atomic/sessions/`.
 */
export interface SessionState {
  /** When this session was first seen (epoch ms) */
  startTime: number;
  /** Number of completed turns (incremented on `session.idle`) */
  turnCount: number;
  /** Last known model identifier (e.g., "claude-sonnet-4-20250514") */
  model?: string;
  /** Last known provider identifier (e.g., "anthropic") */
  provider?: string;
  /** The most recent user prompt text */
  lastPrompt?: string;
  /** Map of tool call ID → start timestamp for duration tracking */
  toolStartTimes: Map<string, number>;
}

// =============================================================================
// CLI Invocation
// =============================================================================

/**
 * Result of invoking the Atomic CLI hook command.
 *
 * The plugin treats all CLI failures as non-fatal — they are logged
 * but never thrown. This type captures the outcome for logging and
 * testing purposes.
 */
export interface HookResult {
  /** Whether the CLI exited successfully (exit code 0) */
  ok: boolean;
  /** The hook verb that was invoked */
  verb: HookVerb;
  /** CLI exit code, if the process ran */
  exitCode?: number;
  /** stderr output on failure */
  error?: string;
  /** Duration of the CLI invocation in milliseconds */
  duration: number;
  /** stderr output (captured even on success for diagnostics) */
  stderr?: string;
  /** stdout output from the CLI */
  stdout?: string;
}

// =============================================================================
// Configuration
// =============================================================================

/**
 * Plugin configuration.
 *
 * Currently all values are compile-time constants (see `constants.ts`),
 * but this type exists so we can support runtime configuration from
 * `opencode.json` or environment variables in the future.
 */
export interface AtomicHooksConfig {
  /** The Atomic CLI binary name or path */
  command: string;
  /** Arguments prepended to every hook invocation */
  hookArgs: readonly string[];
  /** Tool names that modify files on disk */
  editTools: ReadonlySet<string>;
  /** Plugin version injected into shell environment */
  version: string;
  /** Whether to enable debug-level logging */
  debug: boolean;
}
