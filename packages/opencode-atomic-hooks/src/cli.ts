/**
 * Atomic VCS Hooks Plugin — CLI Invocation Layer
 *
 * Encapsulates all interaction with the Atomic CLI binary. Every hook
 * event ultimately flows through `invokeHook()`, which pipes a JSON
 * payload to `atomic agent hooks opencode <verb>` via stdin.
 *
 * ## Design Principles
 *
 * 1. **Best-effort**: CLI failures are logged, never thrown. The AI
 *    session must never be blocked by a provenance recording failure.
 *
 * 2. **Observable**: Every invocation returns a `HookResult` with
 *    timing and error details for structured logging and testing.
 *
 * 3. **Testable**: The `Shell` dependency is injected, so tests can
 *    substitute a mock without touching the filesystem or PATH.
 *
 * ## Example
 *
 * ```ts
 * import { invokeHook, isAtomicAvailable } from "./cli"
 *
 * if (await isAtomicAvailable($, "/my/project")) {
 *   const result = await invokeHook($, "session-start", {
 *     session_id: "abc-123",
 *     cwd: "/my/project",
 *     timestamp: new Date().toISOString(),
 *     source: "startup",
 *   }, "/my/project")
 *
 *   console.log(result.ok, result.duration)
 * }
 * ```
 *
 * @module atomic/cli
 */

import { ATOMIC_CMD, HOOK_ARGS, ATOMIC_DIR } from "./constants";
import type { HookVerb, HookPayload, HookResult, Shell } from "./types";

// =============================================================================
// Hook Invocation
// =============================================================================

/**
 * Invoke `atomic agent hooks opencode <verb>` with a JSON payload on stdin.
 *
 * The payload is serialized to JSON and piped via `echo ... | atomic ...`.
 * The process is run with `nothrow()` so non-zero exit codes are captured
 * rather than thrown. The `quiet()` flag suppresses stdout/stderr echoing
 * to the terminal.
 *
 * @param $         - Bun shell from the plugin context
 * @param verb      - The hook verb (e.g., "session-start", "stop")
 * @param payload   - JSON-serializable payload for the hook
 * @param directory - Working directory for the CLI process
 * @returns A `HookResult` describing the outcome (never throws)
 */
export async function invokeHook(
  $: Shell,
  verb: HookVerb,
  payload: HookPayload,
  directory: string,
): Promise<HookResult> {
  const start = performance.now();
  const json = JSON.stringify(payload);

  try {
    const result =
      await $`echo ${json} | ${ATOMIC_CMD} agent hooks opencode ${verb}`
        .cwd(directory)
        .quiet()
        .nothrow();

    const duration = Math.round(performance.now() - start);
    const ok = result.exitCode === 0;
    const stderr = result.stderr.toString().trim();
    const stdout = result.stdout.toString().trim();

    if (!ok) {
      return {
        ok: false,
        verb,
        exitCode: result.exitCode,
        error: stderr || `exit code ${result.exitCode}`,
        duration,
      };
    }

    return { ok: true, verb, exitCode: 0, duration, stderr, stdout };
  } catch (err) {
    const duration = Math.round(performance.now() - start);
    const message = err instanceof Error ? err.message : String(err);

    return {
      ok: false,
      verb,
      error: message,
      duration,
    };
  }
}

// =============================================================================
// Availability Check
// =============================================================================

/**
 * Check whether the Atomic CLI is installed and the directory is an Atomic repo.
 *
 * Performs two checks in sequence:
 *
 * 1. `atomic --version` exits with code 0 (CLI is in PATH)
 * 2. `.atomic/` directory exists in the project root
 *
 * Both checks use `nothrow()` and `quiet()` to avoid polluting output
 * or throwing on missing binaries.
 *
 * @param $         - Bun shell from the plugin context
 * @param directory - Project root directory to check
 * @returns `true` if both the CLI and the `.atomic/` directory are present
 */
export async function isAtomicAvailable(
  $: Shell,
  directory: string,
): Promise<boolean> {
  try {
    const version = await $`${ATOMIC_CMD} --version`
      .cwd(directory)
      .quiet()
      .nothrow();

    if (version.exitCode !== 0) return false;

    const repo = await $`test -d ${ATOMIC_DIR}`
      .cwd(directory)
      .quiet()
      .nothrow();

    return repo.exitCode === 0;
  } catch {
    return false;
  }
}

// =============================================================================
// Payload Helpers
// =============================================================================

/**
 * Create the base fields common to every hook payload.
 *
 * Centralizes timestamp generation and directory injection so
 * individual handlers don't need to repeat themselves.
 *
 * @param sessionId - The OpenCode session ID
 * @param directory - Project working directory
 * @returns A partial payload with `session_id`, `cwd`, and `timestamp`
 */
export function basePayload(
  sessionId: string,
  directory: string,
): { session_id: string; cwd: string; timestamp: string } {
  return {
    session_id: sessionId,
    cwd: directory,
    timestamp: new Date().toISOString(),
  };
}
