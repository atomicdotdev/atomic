import type { Plugin } from "@opencode-ai/plugin";
import { appendFileSync, mkdirSync } from "fs";
import { join } from "path";

interface ToolCall {
  tool: string;
  callID: string;
  sessionID: string;
  status: string;
  input?: Record<string, unknown>;
  output?: string;
  startTime: number;
}

export const AtomicHooksPlugin: Plugin = async ({ $, directory }) => {
  const ATOMIC = "atomic";
  const logFile = join(directory, ".atomic", "plugin.log");
  try {
    mkdirSync(join(directory, ".atomic"), { recursive: true });
  } catch {}

  const log = (msg: string) => {
    const line = `${new Date().toISOString()} ${msg}\n`;
    try {
      appendFileSync(logFile, line);
    } catch {}
  };

  const hook = async (
    sessionID: string,
    verb: string,
    extra: Record<string, unknown> = {},
  ) => {
    const payload = JSON.stringify({
      session_id: sessionID,
      cwd: directory,
      timestamp: new Date().toISOString(),
      ...extra,
    });
    try {
      const result =
        await $`echo ${payload} | ${ATOMIC} agent hooks opencode ${verb}`
          .cwd(directory)
          .quiet()
          .nothrow();
      const ok = result.exitCode === 0;
      const stderr = result.stderr.toString().trim();
      log(
        `${verb} session=${sessionID} exit=${result.exitCode}${stderr ? " stderr=" + stderr : ""}`,
      );
      return ok;
    } catch (err) {
      log(`${verb} session=${sessionID} error=${err}`);
      return false;
    }
  };

  // Per-session state within this process lifetime
  const sessions = new Map<
    string,
    {
      started: boolean;
      turnCount: number;
      model?: string;
      provider?: string;
      prompt?: string;
      pendingTools: Map<string, ToolCall>;
      completedTools: ToolCall[];
      // Token/cost tracking from step-finish events
      totalInputTokens: number;
      totalOutputTokens: number;
      totalCacheRead: number;
      totalCacheWrite: number;
      totalCost: number;
      // Per-turn accumulator (reset after each record)
      turnInputTokens: number;
      turnOutputTokens: number;
      turnCacheRead: number;
      turnCacheWrite: number;
      turnCost: number;
    }
  >();

  const ensure = (id: string) => {
    if (!sessions.has(id))
      sessions.set(id, {
        started: false,
        turnCount: 0,
        pendingTools: new Map(),
        completedTools: [],
        totalInputTokens: 0,
        totalOutputTokens: 0,
        totalCacheRead: 0,
        totalCacheWrite: 0,
        totalCost: 0,
        turnInputTokens: 0,
        turnOutputTokens: 0,
        turnCacheRead: 0,
        turnCacheWrite: 0,
        turnCost: 0,
      });
    return sessions.get(id)!;
  };

  // Ensure session-start + user-prompt have been sent, then send stop to record
  const recordTurn = async (sessionID: string) => {
    const state = ensure(sessionID);

    if (!state.started) {
      await hook(sessionID, "session-start", { source: "startup" });
      state.started = true;
    }

    state.turnCount++;

    // Use the user's prompt as the change message — that captures intent.
    // Tool summaries belong in the provenance graph, not the change message.
    const turnMsg = state.prompt
      ? state.prompt.substring(0, 200)
      : `Turn ${state.turnCount}`;

    // user-prompt transitions Idle → Active on the Rust side
    await hook(sessionID, "user-prompt", {
      prompt: turnMsg,
      ...(state.model ? { model: state.model } : {}),
      ...(state.provider ? { provider: state.provider } : {}),
    });

    // Send after-tool for each completed tool call since last turn
    // so the Rust-side provenance accumulator gets populated
    for (const tc of state.completedTools) {
      await hook(sessionID, "after-tool", {
        tool_name: tc.tool,
        tool_call_id: tc.callID,
        status: tc.status,
        duration: tc.startTime > 0 ? Date.now() - tc.startTime : undefined,
        modified_files: [
          "edit",
          "write",
          "multiedit",
          "patch",
          "create",
        ].includes(tc.tool),
        tool_input: tc.input,
        tool_output: tc.output?.substring(0, 500),
      });
    }
    state.completedTools = [];

    // stop triggers RecordIfChanged → record_turn
    await hook(sessionID, "stop", {
      turn_number: state.turnCount,
      ...(state.model ? { model: state.model } : {}),
      ...(state.provider ? { provider: state.provider } : {}),
      input_tokens: state.turnInputTokens,
      output_tokens: state.turnOutputTokens,
      cache_read_tokens: state.turnCacheRead,
      cache_write_tokens: state.turnCacheWrite,
      cost_usd: state.turnCost,
    });

    // Roll turn tokens into session totals, reset turn accumulators
    state.totalInputTokens += state.turnInputTokens;
    state.totalOutputTokens += state.turnOutputTokens;
    state.totalCacheRead += state.turnCacheRead;
    state.totalCacheWrite += state.turnCacheWrite;
    state.totalCost += state.turnCost;
    state.turnInputTokens = 0;
    state.turnOutputTokens = 0;
    state.turnCacheRead = 0;
    state.turnCacheWrite = 0;
    state.turnCost = 0;

    // Clear last tool summary for next turn
    state.lastToolSummary = undefined;
  };

  log(`plugin loaded dir=${directory}`);

  return {
    event: async ({ event }: { event: any }) => {
      const type = event.type as string;
      const props = event.properties ?? {};

      // Session created — send session-start to create agent stack
      if (type === "session.created") {
        const sessionID = props.info?.id;
        if (!sessionID) return;
        const state = ensure(sessionID);
        if (!state.started) {
          await hook(sessionID, "session-start", {
            source: "startup",
            ...(state.model ? { model: state.model } : {}),
            ...(state.provider ? { provider: state.provider } : {}),
          });
          state.started = true;
        }
      }

      // Turn completed — agent is done responding to the user's prompt.
      // All file writes are finished. Record once with all accumulated
      // tool calls from the entire turn.
      if (type === "session.idle") {
        const sessionID = props.sessionID;
        if (sessionID) {
          const state = ensure(sessionID);
          if (state.completedTools.length > 0) {
            log(
              `session.idle session=${sessionID} tools=${state.completedTools.length}`,
            );
            await recordTurn(sessionID);
          }
        }
      }

      // Session deleted — finalize: send session-end to create attestation + provenance artifact
      if (type === "session.deleted") {
        const sessionID = props.info?.id;
        if (!sessionID) return;
        await hook(sessionID, "session-end", { reason: "deleted" });
        sessions.delete(sessionID);
      }

      // Session disposed (user closed opencode) — also finalize
      if (type === "server.instance.disposed") {
        for (const [sessionID, state] of sessions) {
          if (state.started) {
            await hook(sessionID, "session-end", { reason: "disposed" });
          }
        }
        sessions.clear();
      }

      // Capture model info from any message that has it
      if (type === "message.updated") {
        const info = props.info;
        if (info?.sessionID) {
          const state = ensure(info.sessionID);
          // Assistant messages carry providerID/modelID at top level
          if (info.providerID && info.modelID) {
            state.provider = info.providerID;
            state.model = info.modelID;
          }
          // User messages carry model as nested object
          if (info.model?.providerID && info.model?.modelID) {
            state.provider = info.model.providerID;
            state.model = info.model.modelID;
          }
        }
      }

      // Capture tokens/cost from step-finish events
      if (type === "message.part.updated") {
        const part = props.part;
        if (!part?.sessionID) return;
        const state = ensure(part.sessionID);

        // step-finish carries per-step token/cost data
        if (part.type === "step-finish" && part.tokens) {
          state.turnInputTokens += part.tokens.input ?? 0;
          state.turnOutputTokens += part.tokens.output ?? 0;
          state.turnCacheRead += part.tokens.cache?.read ?? 0;
          state.turnCacheWrite += part.tokens.cache?.write ?? 0;
          state.turnCost += part.cost ?? 0;
        }

        // First user text = the prompt
        if (part.type === "text" && part.text && !state.prompt) {
          state.prompt = part.text.substring(0, 500);
        }

        // Track tool calls for provenance
        if (part.type === "tool" && part.callID && part.tool) {
          const callID = part.callID as string;

          if (part.state?.status === "running") {
            state.pendingTools.set(callID, {
              tool: part.tool,
              callID,
              sessionID: part.sessionID,
              status: "running",
              input: part.state?.input,
              startTime: Date.now(),
            });
          }

          if (
            part.state?.status === "completed" ||
            part.state?.status === "error"
          ) {
            const pending = state.pendingTools.get(callID);
            const tc: ToolCall = {
              tool: part.tool,
              callID,
              sessionID: part.sessionID,
              status: part.state.status,
              input: pending?.input ?? part.state?.input,
              output: part.state?.output?.substring?.(0, 500),
              startTime: pending?.startTime ?? Date.now(),
            };
            state.pendingTools.delete(callID);
            state.completedTools.push(tc);
          }
        }
      }
    },

    "shell.env": async (_input: any, output: any) => {
      output.env.ATOMIC_AGENT = "opencode";
    },
  };
};
