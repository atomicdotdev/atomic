/**
 * Atomic VCS Hooks Plugin — Handler Barrel Exports
 *
 * Re-exports all handler factory functions and their associated types
 * from a single entry point. This module is the only import needed
 * when assembling the complete `Hooks` object in the plugin entry point.
 *
 * ## Architecture
 *
 * Each handler module follows the same pattern:
 *
 * 1. **Factory function** — Accepts a `Deps` object with injected
 *    dependencies (shell, session store, logger, directory) and returns
 *    a function matching the corresponding OpenCode plugin hook signature.
 *
 * 2. **Types** — Input/output interfaces that mirror the OpenCode plugin
 *    types, declared locally so handler modules don't depend on the full
 *    `@opencode-ai/plugin` package.
 *
 * 3. **Helpers** — Pure functions (e.g., `extractPrompt`, `isEditTool`,
 *    `truncateOutput`) exported for unit testing.
 *
 * ## Handler Summary
 *
 * | Module  | Hook                   | Atomic Verb     | Factory                    |
 * |---------|------------------------|-----------------|----------------------------|
 * | event   | `event`                | session-start,  | `createEventHandler`       |
 * |         |                        | stop,           |                            |
 * |         |                        | session-end     |                            |
 * | chat    | `chat.message`         | user-prompt     | `createChatHandler`        |
 * | tool    | `tool.execute.before`  | before-tool     | `createBeforeToolHandler`  |
 * | tool    | `tool.execute.after`   | after-tool      | `createAfterToolHandler`   |
 * | shell   | `shell.env`            | (env injection) | `createShellHandler`       |
 *
 * ## Usage
 *
 * ```ts
 * import {
 *   createEventHandler,
 *   createChatHandler,
 *   createBeforeToolHandler,
 *   createAfterToolHandler,
 *   createShellHandler,
 * } from "./handlers"
 *
 * // Assemble the Hooks object with shared dependencies
 * const deps = { $, store, log, directory }
 *
 * return {
 *   event: createEventHandler(deps),
 *   "chat.message": createChatHandler(deps),
 *   "tool.execute.before": createBeforeToolHandler(deps),
 *   "tool.execute.after": createAfterToolHandler(deps),
 *   "shell.env": createShellHandler(),
 * }
 * ```
 *
 * @module atomic/handlers
 */

// =============================================================================
// Event Bus Handler
// =============================================================================

export { createEventHandler } from "./event"
export type { EventHandlerDeps } from "./event"

// =============================================================================
// Chat Message Handler
// =============================================================================

export { createChatHandler, extractPrompt } from "./chat"
export type { ChatHandlerDeps, ChatMessageInput, ChatMessageOutput, MessagePart } from "./chat"

// =============================================================================
// Tool Execution Handlers
// =============================================================================

export { createBeforeToolHandler, createAfterToolHandler, isEditTool, truncateOutput } from "./tool"
export type { ToolHandlerDeps, ToolBeforeInput, ToolBeforeOutput, ToolAfterInput, ToolAfterOutput } from "./tool"

// =============================================================================
// Shell Environment Handler
// =============================================================================

export { createShellHandler, defaultShellConfig } from "./shell"
export type { ShellHandlerConfig, ShellEnvInput, ShellEnvOutput } from "./shell"

// =============================================================================
// Compaction Handler
// =============================================================================

export { createCompactionHandler } from "./compaction"
export type { CompactionHandlerDeps, CompactionInput, CompactionOutput } from "./compaction"
