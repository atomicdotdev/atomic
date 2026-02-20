/**
 * Atomic VCS Hooks Plugin — Structured Logging
 *
 * Thin wrapper around the OpenCode SDK `client.app.log()` API that
 * provides a consistent logging interface for all plugin modules.
 *
 * ## Why Not `console.log`?
 *
 * OpenCode's plugin documentation recommends `client.app.log()` over
 * `console.log` for structured logging. This gives us:
 *
 * - Log levels (`debug`, `info`, `warn`, `error`)
 * - Service tagging (all entries tagged `atomic-hooks`)
 * - Structured `extra` metadata (JSON-serializable)
 * - Integration with OpenCode's log viewer
 *
 * ## Usage
 *
 * ```ts
 * import { createLogger } from "./log"
 *
 * const log = createLogger(client)
 *
 * log.info("Session started", { sessionId: "abc-123" })
 * log.warn("CLI not found", { path: "/usr/bin/atomic" })
 * log.error("Hook failed", { verb: "stop", exitCode: 1 })
 * log.debug("Payload sent", { json: "{...}" })
 * ```
 *
 * If the client is unavailable (e.g., during tests or early init),
 * `createLogger` accepts `undefined` and falls back to `console`.
 *
 * @module atomic/log
 */

import { LOG_SERVICE } from "./constants"
import type { Client } from "./types"

// =============================================================================
// Logger Interface
// =============================================================================

/** Log levels supported by OpenCode's `app.log()` API. */
export type LogLevel = "debug" | "info" | "warn" | "error"

/**
 * A structured logger that writes to OpenCode's log system.
 *
 * All methods are fire-and-forget — they never throw and never block
 * the caller. Logging failures are silently swallowed because a
 * logging failure should never break the plugin.
 */
export interface Logger {
  debug(message: string, extra?: Record<string, unknown>): void
  info(message: string, extra?: Record<string, unknown>): void
  warn(message: string, extra?: Record<string, unknown>): void
  error(message: string, extra?: Record<string, unknown>): void
}

// =============================================================================
// Implementation
// =============================================================================

/**
 * Create a `Logger` backed by the OpenCode SDK client.
 *
 * @param client - The OpenCode SDK client, or `undefined` for console fallback
 * @returns A `Logger` instance that writes structured log entries
 */
export function createLogger(client?: Client): Logger {
  function log(level: LogLevel, message: string, extra?: Record<string, unknown>): void {
    if (client) {
      // Fire-and-forget — don't await, don't catch
      client.app
        .log({
          body: {
            service: LOG_SERVICE,
            level,
            message,
            ...(extra ? { extra } : {}),
          },
        })
        .catch(() => {
          // Swallow logging errors — never let logging break the plugin
        })
    } else {
      // Fallback for tests or early initialization
      const prefix = `[${LOG_SERVICE}] [${level}]`
      const suffix = extra ? ` ${JSON.stringify(extra)}` : ""
      switch (level) {
        case "debug":
          console.debug(`${prefix} ${message}${suffix}`)
          break
        case "info":
          console.info(`${prefix} ${message}${suffix}`)
          break
        case "warn":
          console.warn(`${prefix} ${message}${suffix}`)
          break
        case "error":
          console.error(`${prefix} ${message}${suffix}`)
          break
      }
    }
  }

  return {
    debug: (message, extra) => log("debug", message, extra),
    info: (message, extra) => log("info", message, extra),
    warn: (message, extra) => log("warn", message, extra),
    error: (message, extra) => log("error", message, extra),
  }
}
