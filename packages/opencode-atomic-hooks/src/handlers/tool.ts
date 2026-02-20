/**
 * Atomic VCS Hooks Plugin — Tool Execution Handlers
 *
 * Handles the `tool.execute.before` and `tool.execute.after` plugin hooks,
 * which fire around every tool invocation during an AI session. These map
 * to the Atomic `before-tool` (PreToolUse) and `after-tool` (PostToolUse)
 * hook verbs.
 *
 * ## Responsibilities
 *
 * 1. **Track tool timing** — Record the start time of each tool call so
 *    we can compute duration when the tool completes.
 *
 * 2. **Classify file mutations** — Determine whether the tool modifies
 *    files on disk using the `EDIT_TOOLS` constant set. This flag tells
 *    the Atomic CLI whether recording is warranted after the turn.
 *
 * 3. **Truncate output** — Tool output (especially from `bash`) can be
 *    arbitrarily large. We truncate to `MAX_TOOL_OUTPUT_LENGTH` before
 *    sending to avoid blowing up stdin pipe buffers.
 *
 * ## Data Flow
 *
 * ```
 * Agent invokes a tool (e.g., "edit", "bash")
 *       │
 *       ├── tool.execute.before fires
 *       │     ├── Record start time in SessionStore
 *       │     └── atomic agent hooks opencode before-tool
 *       │           stdin: { session_id, tool_name, tool_call_id, tool_input, ... }
 *       │
 *       ▼
 * Tool runs (OpenCode executes it)
 *       │
 *       ├── tool.execute.after fires
 *       │     ├── Compute duration from stored start time
 *       │     ├── Classify as file-modifying or read-only
 *       │     ├── Truncate tool output
 *       │     └── atomic agent hooks opencode after-tool
 *       │           stdin: { session_id, tool_name, status, duration, modified_files, ... }
 *       │
 *       ▼
 * opencode.rs → TurnEvent(PreToolUse | PostToolUse) → TurnOrchestrator
 * ```
 *
 * ## Design
 *
 * Both handlers are factory functions that accept dependencies and return
 * functions matching the OpenCode plugin hook signatures. Dependencies
 * are injected for testability — no module-level state.
 *
 * @module atomic/handlers/tool
 */

import { invokeHook, basePayload } from "../cli"
import { EDIT_TOOLS, MAX_TOOL_OUTPUT_LENGTH } from "../constants"
import type { SessionStore } from "../session"
import type { Logger } from "../log"
import type { Shell, BeforeToolPayload, AfterToolPayload } from "../types"

// =============================================================================
// Types
// =============================================================================

/**
 * Dependencies injected into the tool handlers.
 */
export interface ToolHandlerDeps {
  /** Bun shell for CLI invocation */
  $: Shell
  /** Session state store */
  store: SessionStore
  /** Structured logger */
  log: Logger
  /** Project working directory */
  directory: string
}

/**
 * The `input` parameter shape for `tool.execute.before`.
 *
 * Mirrors the OpenCode plugin type so the handler module doesn't
 * depend on the full `Hooks` type.
 */
export interface ToolBeforeInput {
  /** Tool name (e.g., "edit", "bash", "read") */
  tool: string
  /** OpenCode session ID */
  sessionID: string
  /** Unique identifier for this tool invocation */
  callID: string
}

/**
 * The `output` parameter shape for `tool.execute.before`.
 *
 * Contains the (possibly modified) arguments that will be passed
 * to the tool. Plugins can mutate `output.args` to transform input.
 */
export interface ToolBeforeOutput {
  /** Tool arguments (may be any JSON-serializable value) */
  args: unknown
}

/**
 * The `input` parameter shape for `tool.execute.after`.
 */
export interface ToolAfterInput {
  /** Tool name */
  tool: string
  /** OpenCode session ID */
  sessionID: string
  /** Unique identifier for this tool invocation */
  callID: string
}

/**
 * The `output` parameter shape for `tool.execute.after`.
 *
 * Contains the results of the tool execution. Plugins can mutate
 * these fields to transform what the agent sees.
 */
export interface ToolAfterOutput {
  /** Short title describing what the tool did */
  title: string
  /** The tool's text output */
  output: string
  /** Arbitrary metadata from the tool */
  metadata: unknown
}

// =============================================================================
// Before-Tool Handler
// =============================================================================

/**
 * Create a `tool.execute.before` handler that invokes the Atomic
 * `before-tool` hook.
 *
 * The handler records the tool call start time in the session store
 * for later duration computation, then pipes the tool name and input
 * arguments to the Atomic CLI.
 *
 * @param deps - Injected dependencies
 * @returns An async function matching the `Hooks["tool.execute.before"]` signature
 *
 * @example
 * ```ts
 * const handler = createBeforeToolHandler({
 *   $: ctx.$,
 *   store,
 *   log,
 *   directory: ctx.directory,
 * })
 *
 * // In the plugin hooks object:
 * return { "tool.execute.before": handler }
 * ```
 */
export function createBeforeToolHandler(deps: ToolHandlerDeps) {
  const { $, store, log, directory } = deps

  return async (input: ToolBeforeInput, output: ToolBeforeOutput): Promise<void> => {
    // Record start time for duration tracking
    store.startTool(input.sessionID, input.callID)

    const payload: BeforeToolPayload = {
      ...basePayload(input.sessionID, directory),
      tool_name: input.tool,
      tool_call_id: input.callID,
      tool_input: output.args,
    }

    const result = await invokeHook($, "before-tool", payload, directory)

    if (result.ok) {
      log.debug("before-tool sent", {
        sessionID: input.sessionID,
        tool: input.tool,
        callID: input.callID,
        duration: result.duration,
      })
    } else {
      log.warn(`before-tool failed for ${input.tool}`, {
        sessionID: input.sessionID,
        tool: input.tool,
        callID: input.callID,
        error: result.error,
        exitCode: result.exitCode,
      })
    }
  }
}

// =============================================================================
// After-Tool Handler
// =============================================================================

/**
 * Create a `tool.execute.after` handler that invokes the Atomic
 * `after-tool` hook.
 *
 * The handler computes the tool execution duration from the stored
 * start time, classifies whether the tool modified files, truncates
 * the output, and sends everything to the Atomic CLI.
 *
 * @param deps - Injected dependencies
 * @returns An async function matching the `Hooks["tool.execute.after"]` signature
 *
 * @example
 * ```ts
 * const handler = createAfterToolHandler({
 *   $: ctx.$,
 *   store,
 *   log,
 *   directory: ctx.directory,
 * })
 *
 * // In the plugin hooks object:
 * return { "tool.execute.after": handler }
 * ```
 */
export function createAfterToolHandler(deps: ToolHandlerDeps) {
  const { $, store, log, directory } = deps

  return async (input: ToolAfterInput, output: ToolAfterOutput): Promise<void> => {
    // Compute duration from stored start time
    const duration = store.endTool(input.sessionID, input.callID)

    const payload: AfterToolPayload = {
      ...basePayload(input.sessionID, directory),
      tool_name: input.tool,
      tool_call_id: input.callID,
      status: "completed",
      modified_files: isEditTool(input.tool),
      ...(duration !== undefined ? { duration } : {}),
      ...truncateOutput(output.output),
    }

    const result = await invokeHook($, "after-tool", payload, directory)

    if (result.ok) {
      log.debug("after-tool sent", {
        sessionID: input.sessionID,
        tool: input.tool,
        callID: input.callID,
        duration,
        modified: payload.modified_files,
        hookDuration: result.duration,
      })
    } else {
      log.warn(`after-tool failed for ${input.tool}`, {
        sessionID: input.sessionID,
        tool: input.tool,
        callID: input.callID,
        error: result.error,
        exitCode: result.exitCode,
      })
    }
  }
}

// =============================================================================
// Helpers
// =============================================================================

/**
 * Check whether a tool name is in the file-modifying tools set.
 *
 * @param tool - The tool name to classify
 * @returns `true` if the tool is known to modify files on disk
 */
export function isEditTool(tool: string): boolean {
  return EDIT_TOOLS.has(tool)
}

/**
 * Truncate tool output to the configured maximum length.
 *
 * Returns an object with `tool_output` set to the truncated string,
 * or an empty object if the output is falsy or empty. This is spread
 * into the payload so `tool_output` is only present when non-empty.
 *
 * @param output - Raw tool output string (may be very large)
 * @returns Object with optional `tool_output` field
 */
export function truncateOutput(output: string | undefined): { tool_output?: string } {
  if (!output) return {}

  const trimmed = output.substring(0, MAX_TOOL_OUTPUT_LENGTH)
  if (trimmed.length === 0) return {}

  return { tool_output: trimmed }
}
