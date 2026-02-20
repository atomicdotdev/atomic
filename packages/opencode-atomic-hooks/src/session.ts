/**
 * Atomic VCS Hooks Plugin — Session State Management
 *
 * Tracks in-memory state for each active OpenCode session. This state
 * is ephemeral — it lives only for the duration of the OpenCode process.
 * The Atomic CLI maintains its own durable session state in
 * `.atomic/sessions/`.
 *
 * ## Responsibilities
 *
 * - Create and retrieve session state by ID
 * - Track turn counts (incremented on `session.idle`)
 * - Store model/provider info captured from `chat.message`
 * - Track tool call start times for duration calculation
 * - Clean up session state on `session.deleted`
 *
 * ## Design
 *
 * The session store is a simple `Map<string, SessionState>` with a
 * functional API. There is no persistence — if OpenCode restarts, all
 * session state is lost. This is intentional: the Atomic CLI is the
 * source of truth for session data, and this module is just a
 * correlation buffer to enrich hook payloads with timing and context.
 *
 * ## Example
 *
 * ```ts
 * import { SessionStore } from "./session"
 *
 * const store = SessionStore.create()
 *
 * const session = store.get("abc-123")
 * session.turnCount          // 0
 *
 * store.incrementTurn("abc-123")
 * store.get("abc-123").turnCount  // 1
 *
 * store.setModel("abc-123", "anthropic", "claude-sonnet-4-20250514")
 * store.get("abc-123").provider   // "anthropic"
 *
 * store.startTool("abc-123", "call-42")
 * const duration = store.endTool("abc-123", "call-42")
 * // duration is elapsed ms, or undefined if call-42 wasn't tracked
 *
 * store.remove("abc-123")
 * store.has("abc-123")  // false
 * ```
 *
 * @module atomic/session
 */

import type { SessionState } from "./types"

// =============================================================================
// Factory
// =============================================================================

/**
 * Create a fresh `SessionState` with default values.
 *
 * @param now - Optional timestamp override for testing (default: `Date.now()`)
 * @returns A new session state initialized to zero turns
 */
export function createSessionState(now?: number): SessionState {
  return {
    startTime: now ?? Date.now(),
    turnCount: 0,
    toolStartTimes: new Map(),
  }
}

// =============================================================================
// Session Store
// =============================================================================

/**
 * An in-memory store for tracking active session state.
 *
 * Wraps a `Map<string, SessionState>` with domain-specific methods
 * so callers don't need to manage the map directly. Every public
 * method is pure with respect to the store instance — no global state.
 */
export interface SessionStore {
  /**
   * Get the session state for `id`, creating it if it doesn't exist.
   *
   * This is the primary access method. Callers never need to check
   * existence before reading — `get` always returns a valid state.
   */
  get(id: string): SessionState

  /**
   * Check whether a session exists without creating it.
   */
  has(id: string): boolean

  /**
   * Remove a session from the store. No-op if it doesn't exist.
   */
  remove(id: string): void

  /**
   * Increment the turn count for a session.
   *
   * Called on `session.idle` events. Creates the session if needed.
   *
   * @returns The new turn count after incrementing
   */
  incrementTurn(id: string): number

  /**
   * Update the model and provider for a session.
   *
   * Called from the `chat.message` hook when model info is available.
   * Only updates fields that are non-empty — passing `undefined`
   * preserves the existing value.
   *
   * @param id       - Session ID
   * @param provider - Provider identifier (e.g., "anthropic")
   * @param model    - Model identifier (e.g., "claude-sonnet-4-20250514")
   */
  setModel(id: string, provider?: string, model?: string): void

  /**
   * Store the most recent user prompt for a session.
   *
   * Called from the `chat.message` hook. Only updates if `prompt`
   * is a non-empty string.
   */
  setPrompt(id: string, prompt: string): void

  /**
   * Record the start time of a tool invocation for duration tracking.
   *
   * @param id     - Session ID
   * @param callId - Unique tool call identifier
   * @param now    - Optional timestamp override for testing
   */
  startTool(id: string, callId: string, now?: number): void

  /**
   * Complete a tool invocation and return the elapsed duration.
   *
   * Removes the start time entry from the session. If the tool call
   * was not previously tracked (e.g., plugin restarted mid-call),
   * returns `undefined`.
   *
   * @param id     - Session ID
   * @param callId - Unique tool call identifier
   * @param now    - Optional timestamp override for testing
   * @returns Elapsed time in milliseconds, or `undefined`
   */
  endTool(id: string, callId: string, now?: number): number | undefined

  /**
   * Return the number of tracked sessions.
   */
  size(): number

  /**
   * Remove all sessions from the store.
   */
  clear(): void
}

// =============================================================================
// Implementation
// =============================================================================

/**
 * Create a new `SessionStore` instance.
 *
 * Each call returns an independent store. In production, the plugin
 * creates one store at initialization and passes it to all handlers.
 * In tests, each test creates its own store for isolation.
 */
export function createSessionStore(): SessionStore {
  const sessions = new Map<string, SessionState>()

  function get(id: string): SessionState {
    const existing = sessions.get(id)
    if (existing) return existing

    const state = createSessionState()
    sessions.set(id, state)
    return state
  }

  function has(id: string): boolean {
    return sessions.has(id)
  }

  function remove(id: string): void {
    sessions.delete(id)
  }

  function incrementTurn(id: string): number {
    const session = get(id)
    session.turnCount++
    return session.turnCount
  }

  function setModel(id: string, provider?: string, model?: string): void {
    const session = get(id)
    if (provider) session.provider = provider
    if (model) session.model = model
  }

  function setPrompt(id: string, prompt: string): void {
    if (!prompt) return
    const session = get(id)
    session.lastPrompt = prompt
  }

  function startTool(id: string, callId: string, now?: number): void {
    const session = get(id)
    session.toolStartTimes.set(callId, now ?? Date.now())
  }

  function endTool(id: string, callId: string, now?: number): number | undefined {
    const session = sessions.get(id)
    if (!session) return undefined

    const start = session.toolStartTimes.get(callId)
    if (start === undefined) return undefined

    session.toolStartTimes.delete(callId)
    return (now ?? Date.now()) - start
  }

  function size(): number {
    return sessions.size
  }

  function clear(): void {
    sessions.clear()
  }

  return {
    get,
    has,
    remove,
    incrementTurn,
    setModel,
    setPrompt,
    startTool,
    endTool,
    size,
    clear,
  }
}

// =============================================================================
// Convenience — Module-Level Singleton
// =============================================================================

/**
 * Namespace re-export for convenient `SessionStore.create()` usage.
 *
 * ```ts
 * import { SessionStore } from "./session"
 * const store = SessionStore.create()
 * ```
 */
export const SessionStore = {
  create: createSessionStore,
  createState: createSessionState,
} as const
