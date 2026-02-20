/**
 * Atomic VCS Hooks Plugin — Shell Environment Handler
 *
 * Handles the `shell.env` plugin hook, which fires before every shell
 * command executed by OpenCode — both AI-initiated tool calls (e.g.,
 * `bash` tool) and user-initiated terminal commands.
 *
 * ## Purpose
 *
 * Injects environment variables into the shell so that any Atomic CLI
 * invocations within the shell session (whether from the plugin hooks
 * or from the user manually running `atomic record`) are tagged with
 * the correct agent identity.
 *
 * ## Injected Variables
 *
 * | Variable               | Value      | Purpose                              |
 * |------------------------|------------|--------------------------------------|
 * | `ATOMIC_AGENT`         | `opencode` | Identifies which AI agent is active  |
 * | `ATOMIC_AGENT_VERSION` | `1.0.0`    | Plugin version for debugging/compat  |
 *
 * These variables are read by the Atomic CLI's recording workflow to:
 *
 * 1. Set the correct author format (e.g., `opencode+ab12 <user@example.com>`)
 * 2. Tag the change's provenance with the agent vendor
 * 3. Select the right learnings file (`opencode.md`)
 *
 * ## Design
 *
 * The handler is a pure function — it mutates the `output.env` object
 * provided by OpenCode and returns. No async work, no CLI calls, no
 * side effects beyond the env mutation.
 *
 * The factory pattern is used for consistency with other handlers,
 * even though this handler has minimal dependencies. This allows
 * future extensions (e.g., injecting session-specific env vars)
 * without changing the public API.
 *
 * ## Example
 *
 * ```ts
 * import { createShellHandler } from "./shell"
 *
 * const handler = createShellHandler({
 *   agentName: "opencode",
 *   version: "1.0.0",
 * })
 *
 * // In the plugin hooks object:
 * return { "shell.env": handler }
 * ```
 *
 * @module atomic/handlers/shell
 */

import { AGENT_NAME, PLUGIN_VERSION } from "../constants"

// =============================================================================
// Types
// =============================================================================

/**
 * Configuration for the shell environment handler.
 *
 * Extracted from constants at creation time so tests can override
 * values without touching module-level state.
 */
export interface ShellHandlerConfig {
  /** Agent identifier injected as `ATOMIC_AGENT` */
  agentName: string
  /** Plugin version injected as `ATOMIC_AGENT_VERSION` */
  version: string
}

/**
 * The `input` parameter shape for the `shell.env` hook.
 *
 * Contains the working directory where the shell command will run.
 */
export interface ShellEnvInput {
  /** Current working directory for the shell command */
  cwd: string
}

/**
 * The `output` parameter shape for the `shell.env` hook.
 *
 * Contains the environment variable map that plugins can mutate.
 * Variables added here are merged into the shell's environment
 * before the command executes.
 */
export interface ShellEnvOutput {
  /** Mutable environment variable map */
  env: Record<string, string>
}

// =============================================================================
// Handler Factory
// =============================================================================

/**
 * Create a `shell.env` handler that injects Atomic agent environment
 * variables into every shell command.
 *
 * The returned function is called synchronously before each shell
 * invocation. It mutates `output.env` to add `ATOMIC_AGENT` and
 * `ATOMIC_AGENT_VERSION`.
 *
 * @param config - Optional configuration overrides (defaults to module constants)
 * @returns An async function matching the `Hooks["shell.env"]` signature
 *
 * @example
 * ```ts
 * // Using defaults
 * const handler = createShellHandler()
 *
 * // Using custom values (for testing)
 * const handler = createShellHandler({
 *   agentName: "test-agent",
 *   version: "0.0.0-test",
 * })
 * ```
 */
export function createShellHandler(config?: Partial<ShellHandlerConfig>) {
  const agentName = config?.agentName ?? AGENT_NAME
  const version = config?.version ?? PLUGIN_VERSION

  return async (_input: ShellEnvInput, output: ShellEnvOutput): Promise<void> => {
    output.env.ATOMIC_AGENT = agentName
    output.env.ATOMIC_AGENT_VERSION = version
  }
}

// =============================================================================
// Default Configuration
// =============================================================================

/**
 * Build the default shell handler configuration from module constants.
 *
 * Useful for tests that want to inspect the defaults without importing
 * the individual constants.
 *
 * @returns The default `ShellHandlerConfig`
 */
export function defaultShellConfig(): ShellHandlerConfig {
  return {
    agentName: AGENT_NAME,
    version: PLUGIN_VERSION,
  }
}
