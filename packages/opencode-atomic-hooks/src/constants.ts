/**
 * Atomic VCS Hooks Plugin — Constants & Configuration
 *
 * Compile-time configuration for the Atomic hooks plugin. All values
 * are defined here so they can be imported by any module without
 * circular dependencies.
 *
 * @module atomic/constants
 */

import type { AtomicHooksConfig } from "./types"

// =============================================================================
// CLI
// =============================================================================

/** The Atomic CLI binary name. Override with `ATOMIC_CMD` env var. */
export const ATOMIC_CMD = process.env.ATOMIC_CMD ?? "atomic"

/**
 * Arguments prepended to every hook invocation.
 *
 * The full command is: `atomic agent hooks opencode <verb>`
 */
export const HOOK_ARGS = ["agent", "hooks", "opencode"] as const

// =============================================================================
// Tool Classification
// =============================================================================

/**
 * Tools that modify files on disk.
 *
 * When a tool in this set completes, the `after-tool` payload sets
 * `modified_files: true` so the Atomic CLI knows file changes may
 * have occurred and recording is warranted.
 */
export const EDIT_TOOLS: ReadonlySet<string> = new Set([
  "edit",
  "write",
  "multiedit",
  "patch",
  "bash",
])

// =============================================================================
// Shell Environment
// =============================================================================

/** Agent identifier injected into shell env as `ATOMIC_AGENT`. */
export const AGENT_NAME = "opencode"

/** Plugin version injected into shell env as `ATOMIC_AGENT_VERSION`. */
export const PLUGIN_VERSION = "1.0.0"

// =============================================================================
// Limits
// =============================================================================

/**
 * Maximum length of tool output included in `after-tool` payloads.
 *
 * Tool output can be arbitrarily large (e.g., `bash` output). We
 * truncate to this limit before sending to the Atomic CLI to avoid
 * blowing up stdin pipe buffers.
 */
export const MAX_TOOL_OUTPUT_LENGTH = 500

// =============================================================================
// Logging
// =============================================================================

/** The `service` field used in all `client.app.log()` calls. */
export const LOG_SERVICE = "atomic-hooks"

// =============================================================================
// Paths
// =============================================================================

/** Directory name that indicates an Atomic repository. */
export const ATOMIC_DIR = ".atomic"

// =============================================================================
// Composite Config
// =============================================================================

/**
 * Build a complete configuration object from constants.
 *
 * This exists so modules can accept a single `AtomicHooksConfig`
 * parameter in tests, overriding individual values without touching
 * module-level constants.
 */
export function defaultConfig(): AtomicHooksConfig {
  return {
    command: ATOMIC_CMD,
    hookArgs: HOOK_ARGS,
    editTools: EDIT_TOOLS,
    version: PLUGIN_VERSION,
    debug: process.env.ATOMIC_DEBUG === "1",
  }
}
