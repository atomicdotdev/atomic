/**
 * Atomic VCS Hooks Plugin — Event Bus Handler
 *
 * Routes OpenCode event bus events to the appropriate Atomic hook verbs.
 * This handler subscribes to the plugin's `event` hook and dispatches
 * session lifecycle events to the Atomic CLI.
 *
 * ## Event Mapping
 *
 * | OpenCode Event    | Atomic Verb     | Description                        |
 * |-------------------|-----------------|------------------------------------|
 * | `session.created` | `session-start` | New AI session created             |
 * | `session.idle`    | `stop`          | Agent finished a turn (TurnEnd)    |
 * | `session.deleted` | `session-end`   | Session removed by user            |
 * | `session.error`   | `stop`          | Error recovery — record what we have |
 *
 * ## Design
 *
 * The handler is a pure function that accepts its dependencies (CLI
 * invoker, session store, logger) as arguments. This makes it fully
 * testable without mocking module-level state.
 *
 * All CLI invocations are best-effort — failures are logged but never
 * thrown. The AI session must never be blocked by provenance recording.
 *
 * @module atomic/handlers/event
 */

import type { Event } from "@opencode-ai/sdk";

import { ATOMIC_CMD } from "../constants";
import { invokeHook, basePayload } from "../cli";
import type { SessionStore } from "../session";
import type { Logger } from "../log";
import type {
  Shell,
  SessionStartPayload,
  SessionEndPayload,
  StopPayload,
} from "../types";

// Track which sessions have had session-start sent to the Rust orchestrator.
// Shared between event handler and chat handler to avoid duplicate calls.
// The chat.message hook fires via Plugin.trigger() BEFORE the bus event
// session.created propagates, so we need coordination.
const sessionStartSent = new Set<string>();

/** Mark a session as having received session-start. */
export function markSessionStarted(sessionID: string) {
  sessionStartSent.add(sessionID);
}

/** Check if session-start was already sent for this session. */
export function isSessionStarted(sessionID: string): boolean {
  return sessionStartSent.has(sessionID);
}

// =============================================================================
// Types
// =============================================================================

/**
 * Dependencies injected into the event handler.
 *
 * Keeping these explicit makes the handler easy to test — just pass
 * in mocks for the shell and logger.
 */
export interface EventHandlerDeps {
  /** Bun shell for CLI invocation */
  $: Shell;
  /** Session state store */
  store: SessionStore;
  /** Structured logger */
  log: Logger;
  /** Project working directory */
  directory: string;
}

// =============================================================================
// Handler
// =============================================================================

/**
 * Create an event handler function compatible with the OpenCode plugin
 * `event` hook signature.
 *
 * The returned function is called for **every** event on the OpenCode
 * bus. It inspects `event.type` and dispatches only the session
 * lifecycle events we care about.
 *
 * @param deps - Injected dependencies
 * @returns An async function matching the `Hooks["event"]` signature
 *
 * @example
 * ```ts
 * const handler = createEventHandler({
 *   $: ctx.$,
 *   store,
 *   log,
 *   directory: ctx.directory,
 * })
 *
 * // In the plugin hooks object:
 * return { event: handler }
 * ```
 */
export function createEventHandler(deps: EventHandlerDeps) {
  const { $, store, log, directory } = deps;

  return async ({ event }: { event: Event }) => {
    switch (event.type) {
      case "session.created":
        await handleSessionCreated(event, $, store, log, directory);
        break;

      case "session.idle":
        await handleSessionIdle(event, $, store, log, directory);
        break;

      case "session.deleted":
        await handleSessionDeleted(event, $, store, log, directory);
        break;

      case "session.error":
        await handleSessionError(event, $, store, log, directory);
        break;
    }
  };
}

// =============================================================================
// Individual Event Handlers
// =============================================================================

/**
 * Handle `session.created` — invoke `session-start` hook.
 *
 * Creates a new session in the store and notifies the Atomic CLI
 * that a new AI session has begun. The CLI will create the agent
 * stack and session metadata.
 */
async function handleSessionCreated(
  event: Event,
  $: Shell,
  store: SessionStore,
  log: Logger,
  directory: string,
): Promise<void> {
  const sessionID = extractSessionId(event);
  if (!sessionID) return;

  // Skip if session-start was already sent by the chat.message handler
  // (which fires before the bus event due to Plugin.trigger ordering).
  if (isSessionStarted(sessionID)) {
    return;
  }

  // Touch the session to ensure it exists in the store
  store.get(sessionID);

  const payload: SessionStartPayload = {
    ...basePayload(sessionID, directory),
    source: "startup",
  };

  const result = await invokeHook($, "session-start", payload, directory);

  if (result.ok) {
    markSessionStarted(sessionID);
    log.info(`Session started: ${sessionID}`, {
      sessionID,
      duration: result.duration,
    });
  } else {
    log.warn(`session-start failed for ${sessionID}`, {
      sessionID,
      error: result.error,
      exitCode: result.exitCode,
    });
  }
}

/**
 * Handle `session.idle` — invoke `stop` hook (TurnEnd).
 *
 * This is the primary recording trigger. When the agent goes idle
 * after completing work, we send the `stop` verb to the orchestrator
 * which runs: status → add untracked → record all on the agent stack.
 *
 * The orchestrator targets the agent stack internally via
 * `RecordOptions::stack()` — no need to switch stacks or call
 * `atomic record` directly.
 *
 * IMPORTANT: We must ensure session-start AND user-prompt were sent
 * before stop, because the orchestrator's state machine requires
 * Idle → Active (TurnStart) → Idle (TurnEnd + RecordTurn).
 * Without TurnStart, TurnEnd from Idle is a no-op.
 */
async function handleSessionIdle(
  event: Event,
  $: Shell,
  store: SessionStore,
  log: Logger,
  directory: string,
): Promise<void> {
  const sessionID = extractSessionId(event);
  if (!sessionID) return;

  const turn = store.incrementTurn(sessionID);
  const session = store.get(sessionID);

  // Ensure session-start was sent (may not have been if chat.message
  // didn't fire — e.g. the agent responded without the user sending
  // a message first, or the session was created outside our plugin).
  if (!isSessionStarted(sessionID)) {
    const startPayload: SessionStartPayload = {
      ...basePayload(sessionID, directory),
      source: "startup",
    };
    const startResult = await invokeHook(
      $,
      "session-start",
      startPayload,
      directory,
    );
    if (startResult.ok) {
      markSessionStarted(sessionID);
    }

    // Also send user-prompt to transition Idle → Active.
    // Without this, the stop hook hits TurnEnd from Idle = no-op.
    const promptPayload = {
      ...basePayload(sessionID, directory),
      ...(session.lastPrompt ? { prompt: session.lastPrompt } : {}),
      ...(session.model ? { model: session.model } : {}),
      ...(session.provider ? { provider: session.provider } : {}),
    };
    await invokeHook($, "user-prompt", promptPayload, directory);
  }

  const payload: StopPayload = {
    ...basePayload(sessionID, directory),
    turn_number: turn,
    ...(session.model ? { model: session.model } : {}),
    ...(session.provider ? { provider: session.provider } : {}),
  };

  const result = await invokeHook($, "stop", payload, directory);

  if (result.ok) {
    const detail: Record<string, unknown> = {
      sessionID,
      turn,
      duration: result.duration,
    };
    if (result.stderr) detail.stderr = result.stderr;
    log.info(`Turn ${turn} recorded for session ${sessionID}`, detail);
  } else {
    log.warn(`stop (turn ${turn}) failed for ${sessionID}`, {
      sessionID,
      turn,
      error: result.error,
      exitCode: result.exitCode,
    });
  }
}

/**
 * Handle `session.deleted` — invoke `session-end` hook.
 *
 * Notifies the Atomic CLI that the session has ended, then removes
 * the session from the in-memory store to free resources.
 */
async function handleSessionDeleted(
  event: Event,
  $: Shell,
  store: SessionStore,
  log: Logger,
  directory: string,
): Promise<void> {
  const sessionID = extractSessionId(event);
  if (!sessionID) return;

  const payload: SessionEndPayload = {
    ...basePayload(sessionID, directory),
    reason: "deleted",
  };

  const result = await invokeHook($, "session-end", payload, directory);

  if (!result.ok) {
    log.warn(`session-end failed for ${sessionID}`, {
      sessionID,
      error: result.error,
      exitCode: result.exitCode,
    });
  }

  store.remove(sessionID);
}

/**
 * Handle `session.error` — best-effort `stop` to salvage the turn.
 *
 * When a session encounters an error, we attempt to record whatever
 * changes the agent made before the error. This only fires if the
 * session has completed at least one turn (otherwise there's nothing
 * to record).
 */
async function handleSessionError(
  event: Event,
  $: Shell,
  store: SessionStore,
  log: Logger,
  directory: string,
): Promise<void> {
  const sessionID = extractSessionId(event);
  if (!sessionID) return;

  // Only attempt recovery if we have a session with turns
  if (!store.has(sessionID)) return;
  const session = store.get(sessionID);
  if (session.turnCount === 0) return;

  const payload: StopPayload = {
    ...basePayload(sessionID, directory),
    turn_number: session.turnCount,
    error: true,
    ...(session.model ? { model: session.model } : {}),
    ...(session.provider ? { provider: session.provider } : {}),
  };

  const result = await invokeHook($, "stop", payload, directory);

  if (result.ok) {
    log.info(`Error recovery recorded for session ${sessionID}`, {
      sessionID,
      turn: session.turnCount,
    });
  } else {
    log.warn(`Error recovery failed for ${sessionID}`, {
      sessionID,
      error: result.error,
    });
  }
}

// =============================================================================
// Helpers
// =============================================================================

/**
 * Extract the session ID from an OpenCode event's properties.
 *
 * OpenCode events carry their data in `event.properties`, but the
 * shape varies by event type:
 *
 * - `session.created` / `session.updated` / `session.deleted`:
 *     `{ info: { id: string, slug: string, ... } }`
 *
 * - `session.idle` / `session.error`:
 *     `{ sessionID: string }`
 *
 * Returns `undefined` if the ID can't be extracted.
 */
function extractSessionId(event: Event): string | undefined {
  const props = event.properties as Record<string, unknown> | undefined;
  if (!props) return undefined;

  // session.created / session.updated / session.deleted → properties.info.id
  const info = props.info as Record<string, unknown> | undefined;
  if (info) {
    const id = info.id;
    if (typeof id === "string" && id.length > 0) return id;
  }

  // session.idle / session.error → properties.sessionID
  const sessionID = props.sessionID;
  if (typeof sessionID === "string" && sessionID.length > 0) return sessionID;

  // Fallback: properties.id (in case the shape changes)
  const id = props.id;
  if (typeof id === "string" && id.length > 0) return id;

  return undefined;
}
