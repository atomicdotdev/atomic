/**
 * Atomic VCS Hooks Plugin — Chat Message Handler
 *
 * Handles the `chat.message` plugin hook, which fires whenever a new
 * user message is sent to the AI. This is the most reliable signal
 * that a new turn is beginning, so we map it to the Atomic `user-prompt`
 * verb (HookType::TurnStart on the Rust side).
 *
 * ## Responsibilities
 *
 * 1. **Capture model info** — Extract `providerID` and `modelID` from
 *    the message input and store them in the session for later use by
 *    the `stop` handler's provenance payload.
 *
 * 2. **Extract prompt text** — Concatenate all `text` parts from the
 *    user message into a single prompt string for the Atomic CLI.
 *
 * 3. **Invoke `user-prompt`** — Pipe the prompt, model, and provider
 *    to `atomic agent hooks opencode user-prompt` via stdin JSON.
 *
 * ## Design
 *
 * Like all handlers, the chat handler is a factory function that
 * accepts its dependencies and returns a function matching the
 * OpenCode plugin hook signature. Dependencies are injected for
 * testability — no module-level state.
 *
 * ## Data Flow
 *
 * ```
 * User types a prompt in OpenCode
 *       │
 *       ▼
 * chat.message hook fires
 *       │
 *       ├── Store model/provider in SessionStore
 *       ├── Extract prompt text from message parts
 *       │
 *       ▼
 * atomic agent hooks opencode user-prompt
 *   stdin: { session_id, prompt, model, provider, cwd, timestamp }
 *       │
 *       ▼
 * opencode.rs → TurnEvent(TurnStart) → TurnOrchestrator
 * ```
 *
 * @module atomic/handlers/chat
 */

import { invokeHook, basePayload } from "../cli";
import { markSessionStarted, isSessionStarted } from "./event";
import type { SessionStore } from "../session";
import type { Logger } from "../log";
import type { Shell, UserPromptPayload, SessionStartPayload } from "../types";

// =============================================================================
// Types
// =============================================================================

/**
 * Dependencies injected into the chat message handler.
 */
export interface ChatHandlerDeps {
  /** Bun shell for CLI invocation */
  $: Shell;
  /** Session state store */
  store: SessionStore;
  /** Structured logger */
  log: Logger;
  /** Project working directory */
  directory: string;
}

/**
 * The `input` parameter shape for the `chat.message` hook.
 *
 * This mirrors the OpenCode plugin type but is declared here so
 * the handler module doesn't depend on the full `Hooks` type.
 */
export interface ChatMessageInput {
  sessionID: string;
  agent?: string;
  model?: { providerID: string; modelID: string };
  messageID?: string;
  variant?: string;
}

/**
 * A single part within a user message.
 *
 * We only care about `text` parts for prompt extraction. Other part
 * types (images, tool results, etc.) are ignored.
 */
export interface MessagePart {
  type: string;
  text?: string;
  [key: string]: unknown;
}

/**
 * The `output` parameter shape for the `chat.message` hook.
 */
export interface ChatMessageOutput {
  message: unknown;
  parts: MessagePart[];
}

// =============================================================================
// Handler Factory
// =============================================================================

/**
 * Create a chat message handler compatible with the OpenCode plugin
 * `chat.message` hook signature.
 *
 * The returned function is called every time a user sends a new message
 * to the AI. It captures model metadata, extracts the prompt text, and
 * invokes the Atomic `user-prompt` hook.
 *
 * @param deps - Injected dependencies
 * @returns An async function matching the `Hooks["chat.message"]` signature
 *
 * @example
 * ```ts
 * const handler = createChatHandler({
 *   $: ctx.$,
 *   store,
 *   log,
 *   directory: ctx.directory,
 * })
 *
 * // In the plugin hooks object:
 * return { "chat.message": handler }
 * ```
 */
export function createChatHandler(deps: ChatHandlerDeps) {
  const { $, store, log, directory } = deps;

  return async (
    input: ChatMessageInput,
    output: ChatMessageOutput,
  ): Promise<void> => {
    const sessionID = input.sessionID;

    // -----------------------------------------------------------------
    // 0. Ensure session-start was sent before user-prompt
    //
    // chat.message fires via Plugin.trigger() BEFORE the bus event
    // session.created propagates to our event handler. Without this,
    // user-prompt arrives first and the Rust orchestrator creates the
    // session without forking the agent stack from the parent.
    // -----------------------------------------------------------------
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
        log.info(`Session started (from chat): ${sessionID}`, {
          sessionID,
          duration: startResult.duration,
        });
      } else {
        log.warn(`session-start (from chat) failed for ${sessionID}`, {
          sessionID,
          error: startResult.error,
        });
      }
    }

    // -----------------------------------------------------------------
    // 1. Capture model/provider info for provenance
    // -----------------------------------------------------------------
    if (input.model) {
      store.setModel(sessionID, input.model.providerID, input.model.modelID);
    }

    // -----------------------------------------------------------------
    // 2. Extract prompt text from message parts
    // -----------------------------------------------------------------
    const prompt = extractPrompt(output.parts);

    if (prompt) {
      store.setPrompt(sessionID, prompt);
    }

    // -----------------------------------------------------------------
    // 3. Build and send the user-prompt payload
    // -----------------------------------------------------------------
    const session = store.get(sessionID);

    const payload: UserPromptPayload = {
      ...basePayload(sessionID, directory),
      ...(prompt ? { prompt } : {}),
      ...(session.model ? { model: session.model } : {}),
      ...(session.provider ? { provider: session.provider } : {}),
    };

    const result = await invokeHook($, "user-prompt", payload, directory);

    if (result.ok) {
      log.info("user-prompt sent", {
        sessionID,
        hasPrompt: !!prompt,
        model: session.model,
        duration: result.duration,
        stderr: result.stderr,
      });
    } else {
      log.warn(`user-prompt failed for ${sessionID}`, {
        sessionID,
        error: result.error,
        exitCode: result.exitCode,
        stderr: result.stderr,
      });
    }
  };
}

// =============================================================================
// Helpers
// =============================================================================

/**
 * Extract prompt text from an array of message parts.
 *
 * Concatenates the `text` field of all parts with `type === "text"`,
 * separated by newlines. Returns `undefined` if no text parts exist
 * or if the result is empty after trimming.
 *
 * @param parts - The message parts from the `chat.message` output
 * @returns The combined prompt text, or `undefined` if empty
 */
export function extractPrompt(parts: MessagePart[]): string | undefined {
  const texts = parts
    .filter((p) => p.type === "text" && typeof p.text === "string")
    .map((p) => p.text as string);

  const combined = texts.join("\n").trim();
  return combined.length > 0 ? combined : undefined;
}
