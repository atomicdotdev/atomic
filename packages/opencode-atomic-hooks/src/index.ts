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

// DEBUG: Safe deep-inspect that handles circular refs + truncates big strings
function inspect(value: unknown, maxStringLen = 500, maxDepth = 6): string {
  const seen = new WeakSet();
  function walk(v: unknown, depth: number): unknown {
    if (depth > maxDepth) return "[max depth]";
    if (v === null || v === undefined) return v;
    if (typeof v === "string")
      return v.length > maxStringLen
        ? v.substring(0, maxStringLen) + `...[${v.length} chars]`
        : v;
    if (typeof v === "number" || typeof v === "boolean") return v;
    if (typeof v === "function") return `[function ${v.name || "anonymous"}]`;
    if (typeof v === "symbol") return v.toString();
    if (typeof v === "bigint") return v.toString();
    if (v instanceof Date) return v.toISOString();
    if (v instanceof RegExp) return v.toString();
    if (v instanceof Error)
      return { name: v.name, message: v.message, stack: v.stack };
    if (typeof v === "object") {
      if (seen.has(v as object)) return "[circular]";
      seen.add(v as object);
      if (Array.isArray(v))
        return v.length > 50
          ? [
              ...v.slice(0, 50).map((x) => walk(x, depth + 1)),
              `...[${v.length} items]`,
            ]
          : v.map((x) => walk(x, depth + 1));
      const out: Record<string, unknown> = {};
      for (const key of Object.keys(v as Record<string, unknown>)) {
        out[key] = walk((v as Record<string, unknown>)[key], depth + 1);
      }
      return out;
    }
    return String(v);
  }
  try {
    return JSON.stringify(walk(value, 0), null, 2);
  } catch {
    return `[inspect error: ${typeof value}]`;
  }
}

export const AtomicHooksPlugin: Plugin = async ({ $, directory }) => {
  const ATOMIC = "atomic";
  const atomicDir = join(directory, ".atomic");
  const logFile = join(atomicDir, "plugin.log");
  const debugFile = join(atomicDir, "plugin-debug.log");
  try {
    mkdirSync(atomicDir, { recursive: true });
  } catch {}

  // Set of messageIDs that belong to user messages (from chat.message hook).
  // Used to distinguish user text parts from assistant text parts in the event stream.
  const userMessageIDs = new Set<string>();

  const log = (msg: string) => {
    const line = `${new Date().toISOString()} ${msg}\n`;
    try {
      appendFileSync(logFile, line);
    } catch {}
  };

  // DEBUG: Separate debug log with full object inspection
  const debug = (label: string, data?: unknown) => {
    const ts = new Date().toISOString();
    const body = data !== undefined ? `\n${inspect(data)}` : "";
    try {
      appendFileSync(debugFile, `${ts} [${label}]${body}\n---\n`);
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
      agent?: string;
      prompt?: string;
      userMessageID?: string;
      pendingTools: Map<string, ToolCall>;
      completedTools: ToolCall[];
      totalInputTokens: number;
      totalOutputTokens: number;
      totalCacheRead: number;
      totalCacheWrite: number;
      totalReasoningTokens: number;
      totalCost: number;
      turnInputTokens: number;
      turnOutputTokens: number;
      turnCacheRead: number;
      turnCacheWrite: number;
      turnReasoningTokens: number;
      turnCost: number;
      // Step tracking
      turnStepCount: number;
      lastFinishReason?: string;
      // Turn timing — real wall-clock duration from chat.message to session.idle
      turnStartedAt?: number;
      // Session metadata
      sessionSlug?: string;
      // Reasoning accumulation (per turn)
      turnReasoningBlocks: {
        text: string;
        durationMs: number;
        signature?: string;
      }[];
      // Todo plan (latest snapshot)
      todos?: { content: string; status: string; priority: string }[];
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
        totalReasoningTokens: 0,
        totalCost: 0,
        turnInputTokens: 0,
        turnOutputTokens: 0,
        turnCacheRead: 0,
        turnCacheWrite: 0,
        turnReasoningTokens: 0,
        turnCost: 0,
        turnStepCount: 0,
        turnStartedAt: undefined,
        turnReasoningBlocks: [],
      });
    return sessions.get(id)!;
  };

  const recordTurn = async (sessionID: string) => {
    const state = ensure(sessionID);

    if (!state.started) {
      await hook(sessionID, "session-start", { source: "startup" });
      state.started = true;
    }

    state.turnCount++;

    const turnMsg = state.prompt
      ? state.prompt.substring(0, 200)
      : `Turn ${state.turnCount}`;

    await hook(sessionID, "user-prompt", {
      prompt: turnMsg,
      ...(state.model ? { model: state.model } : {}),
      ...(state.provider ? { provider: state.provider } : {}),
      ...(state.agent ? { agent: state.agent } : {}),
    });

    for (const tc of state.completedTools) {
      const extra: Record<string, unknown> = tc as any;
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
        // Rich metadata from tool.execute.after hook
        ...(extra.title ? { title: extra.title } : {}),
        ...(extra.filePath ? { file_path: extra.filePath } : {}),
        ...(extra.filediff ? { filediff: extra.filediff } : {}),
        ...(extra.diagnostics ? { diagnostics: extra.diagnostics } : {}),
        ...(extra.exitCode !== undefined ? { exit_code: extra.exitCode } : {}),
      });
    }
    state.completedTools = [];

    // Build concatenated reasoning text and extract last signature
    const reasoningText = state.turnReasoningBlocks
      .map((b) => b.text)
      .filter((t) => t.length > 0)
      .join("\n---\n");
    const lastSignature = [...state.turnReasoningBlocks]
      .reverse()
      .find((b) => b.signature)?.signature;
    const totalReasoningDurationMs = state.turnReasoningBlocks.reduce(
      (sum, b) => sum + b.durationMs,
      0,
    );

    // Build structured array with per-block metadata for richer provenance nodes
    const reasoningBlocksPayload =
      state.turnReasoningBlocks.length > 0
        ? state.turnReasoningBlocks
            .filter((b) => b.text.length > 0)
            .map((b) => ({
              text: b.text,
              duration_ms: b.durationMs,
              ...(b.signature ? { signature: b.signature } : {}),
            }))
        : undefined;

    await hook(sessionID, "stop", {
      turn_number: state.turnCount,
      ...(state.model ? { model: state.model } : {}),
      ...(state.provider ? { provider: state.provider } : {}),
      ...(state.agent ? { agent: state.agent } : {}),
      input_tokens: state.turnInputTokens,
      output_tokens: state.turnOutputTokens,
      cache_read_tokens: state.turnCacheRead,
      cache_write_tokens: state.turnCacheWrite,
      reasoning_tokens: state.turnReasoningTokens,
      cost_usd: state.turnCost,
      // Actual wall-clock turn duration (chat.message → session.idle)
      ...(state.turnStartedAt
        ? { turn_duration_ms: Date.now() - state.turnStartedAt }
        : {}),
      // New enriched fields
      step_count: state.turnStepCount,
      ...(state.lastFinishReason
        ? { finish_reason: state.lastFinishReason }
        : {}),
      ...(state.sessionSlug ? { session_slug: state.sessionSlug } : {}),
      ...(reasoningText ? { reasoning_text: reasoningText } : {}),
      ...(lastSignature ? { reasoning_signature: lastSignature } : {}),
      ...(totalReasoningDurationMs > 0
        ? { reasoning_duration_ms: totalReasoningDurationMs }
        : {}),
      ...(reasoningBlocksPayload
        ? { reasoning_blocks: reasoningBlocksPayload }
        : {}),
      ...(state.todos ? { todos: state.todos } : {}),
    });

    const durationMs = state.turnStartedAt
      ? Date.now() - state.turnStartedAt
      : 0;
    const durationSec = (durationMs / 1000).toFixed(1);

    log(
      `turn ${state.turnCount} session=${sessionID} prompt="${turnMsg}" model=${state.model} provider=${state.provider} agent=${state.agent} steps=${state.turnStepCount} finish=${state.lastFinishReason ?? "?"} duration=${durationSec}s tokens={in:${state.turnInputTokens},out:${state.turnOutputTokens},reasoning:${state.turnReasoningTokens},cache_r:${state.turnCacheRead},cache_w:${state.turnCacheWrite}} cost=$${state.turnCost.toFixed(4)} reasoning_blocks=${state.turnReasoningBlocks.length}${lastSignature ? " ✓signed" : ""}`,
    );

    state.totalInputTokens += state.turnInputTokens;
    state.totalOutputTokens += state.turnOutputTokens;
    state.totalCacheRead += state.turnCacheRead;
    state.totalCacheWrite += state.turnCacheWrite;
    state.totalReasoningTokens += state.turnReasoningTokens;
    state.totalCost += state.turnCost;
    state.turnInputTokens = 0;
    state.turnOutputTokens = 0;
    state.turnCacheRead = 0;
    state.turnCacheWrite = 0;
    state.turnReasoningTokens = 0;
    state.turnCost = 0;
    state.turnStepCount = 0;
    state.turnStartedAt = undefined;
    state.lastFinishReason = undefined;
    state.turnReasoningBlocks = [];

    state.prompt = undefined;
    state.userMessageID = undefined;
  };

  log(`plugin loaded dir=${directory}`);
  debug("PLUGIN_LOADED", { directory, atomicCmd: ATOMIC });

  return {
    event: async ({ event }: { event: any }) => {
      const type = event.type as string;
      const props = event.properties ?? {};

      // DEBUG: Dump EVERY event — this is the firehose view
      debug(`EVENT:${type}`, { type, properties: props });

      // Session created
      if (type === "session.created") {
        const sessionID = props.info?.id;
        debug("SESSION_CREATED_INFO", props.info);
        // Capture session slug at creation
        if (sessionID && props.info?.slug) {
          const st = ensure(sessionID);
          st.sessionSlug = props.info.slug;
        }
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

      // Turn completed
      if (type === "session.idle") {
        const sessionID = props.sessionID;
        if (sessionID) {
          const state = ensure(sessionID);
          // DEBUG: Log idle with current state snapshot
          debug("SESSION_IDLE_STATE", {
            sessionID,
            turnCount: state.turnCount,
            completedToolCount: state.completedTools.length,
            pendingToolCount: state.pendingTools.size,
            model: state.model,
            provider: state.provider,
            turnTokens: {
              input: state.turnInputTokens,
              output: state.turnOutputTokens,
              cacheRead: state.turnCacheRead,
              cacheWrite: state.turnCacheWrite,
              cost: state.turnCost,
            },
            prompt: state.prompt?.substring(0, 100),
          });
          if (state.completedTools.length > 0) {
            log(
              `session.idle session=${sessionID} tools=${state.completedTools.length}`,
            );
            await recordTurn(sessionID);
          }
        }
      }

      // Session deleted
      if (type === "session.deleted") {
        // DEBUG: Dump full Session object on delete too
        debug("SESSION_DELETED_INFO", props.info);
        const sessionID = props.info?.id;
        if (!sessionID) return;
        await hook(sessionID, "session-end", { reason: "deleted" });
        sessions.delete(sessionID);
      }

      // Session disposed
      if (type === "server.instance.disposed") {
        debug("SERVER_DISPOSED", props);
        for (const [sessionID, state] of sessions) {
          if (state.started) {
            await hook(sessionID, "session-end", { reason: "disposed" });
          }
        }
        sessions.clear();
      }

      // Capture model info from messages
      if (type === "message.updated") {
        const info = props.info;
        // DEBUG: Dump full Message (UserMessage | AssistantMessage)
        debug("MESSAGE_UPDATED_INFO", info);
        if (info?.sessionID) {
          const state = ensure(info.sessionID);
          if (info.providerID && info.modelID) {
            state.provider = info.providerID;
            state.model = info.modelID;
          }
          if (info.model?.providerID && info.model?.modelID) {
            state.provider = info.model.providerID;
            state.model = info.model.modelID;
          }
          // DEBUG: Log assistant message fields we might be missing
          if (info.role === "assistant") {
            debug("ASSISTANT_MESSAGE_FIELDS", {
              sessionID: info.sessionID,
              modelID: info.modelID,
              providerID: info.providerID,
              mode: info.mode,
              cost: info.cost,
              tokens: info.tokens,
              finish: info.finish,
              path: info.path,
              parentID: info.parentID,
              summary: info.summary,
              error: info.error,
              time: info.time,
            });
          }
        }
      }

      // Message parts — the richest data source
      if (type === "message.part.updated") {
        const part = props.part;
        const delta = props.delta;
        // DEBUG: Dump full Part union + delta
        debug(`PART:${part?.type ?? "unknown"}`, { part, delta });
        if (!part?.sessionID) return;
        const state = ensure(part.sessionID);

        // step-finish — tokens, cost, reasoning tokens, step count, finish reason
        if (part.type === "step-finish") {
          debug("STEP_FINISH_DETAIL", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            reason: part.reason,
            cost: part.cost,
            tokens: part.tokens,
            snapshot: part.snapshot,
          });
          state.turnStepCount++;
          if (part.reason) {
            state.lastFinishReason = part.reason;
          }
          if (part.tokens) {
            state.turnInputTokens += part.tokens.input ?? 0;
            state.turnOutputTokens += part.tokens.output ?? 0;
            state.turnCacheRead += part.tokens.cache?.read ?? 0;
            state.turnCacheWrite += part.tokens.cache?.write ?? 0;
            state.turnReasoningTokens += part.tokens.reasoning ?? 0;
            state.turnCost += part.cost ?? 0;
          }
        }

        // step-start — snapshot before the step
        if (part.type === "step-start") {
          debug("STEP_START_DETAIL", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            snapshot: part.snapshot,
          });
        }

        // text part — only capture user message text as the prompt.
        // Assistant text parts share the same structure but have a different messageID.
        // We track user messageIDs from the chat.message hook to distinguish them.
        if (part.type === "text") {
          const isUserText = userMessageIDs.has(part.messageID);
          debug("TEXT_PART_DETAIL", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            textLen: part.text?.length,
            isUserText,
            synthetic: part.synthetic,
            ignored: part.ignored,
            time: part.time,
            metadata: part.metadata,
          });
          // Only capture text from user messages as the prompt.
          // Without this guard, assistant summary text overwrites the user's actual intent.
          if (part.text && isUserText) {
            state.prompt = part.text.substring(0, 500);
          }
        }

        // reasoning part — thinking/chain-of-thought
        // Accumulate completed reasoning blocks (those with time.end set)
        // for inclusion in the stop payload provenance.
        if (part.type === "reasoning") {
          debug("REASONING_PART", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            text: part.text,
            textLen: part.text?.length,
            delta,
            time: part.time,
            metadata: part.metadata,
          });
          // Only accumulate completed blocks (time.end is set)
          if (part.text && part.time?.end) {
            const durationMs =
              part.time.start && part.time.end
                ? part.time.end - part.time.start
                : 0;
            const signature = part.metadata?.anthropic?.signature;
            state.turnReasoningBlocks.push({
              text: part.text,
              durationMs,
              ...(signature ? { signature } : {}),
            });
          }
        }

        // patch part — file changes with hash!
        if (part.type === "patch") {
          debug("PATCH_PART", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            hash: part.hash,
            files: part.files,
          });
        }

        // snapshot part
        if (part.type === "snapshot") {
          debug("SNAPSHOT_PART", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            snapshot: part.snapshot,
          });
        }

        // agent part — sub-agent invocation
        if (part.type === "agent") {
          debug("AGENT_PART", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            name: part.name,
            source: part.source,
          });
        }

        // compaction part
        if (part.type === "compaction") {
          debug("COMPACTION_PART", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            auto: part.auto,
          });
        }

        // retry part — error + retry info
        if (part.type === "retry") {
          debug("RETRY_PART", {
            sessionID: part.sessionID,
            attempt: part.attempt,
            error: part.error,
            time: part.time,
          });
        }

        // file part — file references
        if (part.type === "file") {
          debug("FILE_PART", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            mime: part.mime,
            filename: part.filename,
            url: part.url,
            source: part.source,
          });
        }

        // tool part — track tool calls for provenance
        if (part.type === "tool" && part.callID && part.tool) {
          // DEBUG: Dump the FULL tool part with all state fields
          debug("TOOL_PART_DETAIL", {
            sessionID: part.sessionID,
            messageID: part.messageID,
            callID: part.callID,
            tool: part.tool,
            stateStatus: part.state?.status,
            stateInput: part.state?.input,
            stateOutput: part.state?.output?.substring?.(0, 300),
            stateTitle: part.state?.title,
            stateMetadata: part.state?.metadata,
            stateTime: part.state?.time,
            stateError: part.state?.error,
            stateAttachments: part.state?.attachments,
            partMetadata: part.metadata,
          });

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

      // DEBUG: Events we're NOT handling yet but might want to
      if (type === "session.diff") {
        debug("SESSION_DIFF", props);
      }
      if (type === "session.error") {
        debug("SESSION_ERROR", props);
      }
      if (type === "session.status") {
        debug("SESSION_STATUS", props);
      }
      if (type === "session.compacted") {
        debug("SESSION_COMPACTED", props);
      }
      if (type === "file.edited") {
        debug("FILE_EDITED", props);
      }
      if (type === "file.watcher.updated") {
        debug("FILE_WATCHER", props);
      }
      if (type === "todo.updated") {
        debug("TODO_UPDATED", props);
        // Capture latest todo snapshot for provenance
        if (props.sessionID && props.todos) {
          const st = ensure(props.sessionID);
          st.todos = props.todos;
        }
      }
      if (type === "command.executed") {
        debug("COMMAND_EXECUTED", props);
      }
      if (type === "session.updated") {
        debug("SESSION_UPDATED_INFO", props.info);
        // Capture session slug from session updates
        if (props.info?.slug) {
          const sid = props.info?.id;
          if (sid) {
            const st = ensure(sid);
            if (!st.sessionSlug) {
              st.sessionSlug = props.info.slug;
            }
          }
        }
      }
      if (type === "permission.updated") {
        debug("PERMISSION_UPDATED", props);
      }
      if (type === "permission.replied") {
        debug("PERMISSION_REPLIED", props);
      }
    },

    // chat.message fires when the user sends a prompt — this is the authoritative
    // source for the user's intent, model, provider, and agent mode.
    "chat.message": async (input: any, output: any) => {
      debug("CHAT_MESSAGE_INPUT", input);
      debug("CHAT_MESSAGE_OUTPUT", {
        message: output.message,
        partsCount: output.parts?.length,
        parts: output.parts,
      });

      const sessionID = input.sessionID;
      if (!sessionID) return;
      const state = ensure(sessionID);

      // Mark the real start of this turn (wall-clock)
      state.turnStartedAt = Date.now();

      // Store model/provider/agent from the authoritative chat.message hook
      if (input.model) {
        state.model = input.model.modelID;
        state.provider = input.model.providerID;
      }
      if (input.agent) {
        state.agent = input.agent;
      }

      // Track user messageID so we can distinguish user text parts from assistant text parts
      if (input.messageID) {
        state.userMessageID = input.messageID;
        userMessageIDs.add(input.messageID);
      }

      // Extract the user's prompt from the message parts — this is the real intent
      const prompt = output.parts
        ?.filter((p: any) => p.type === "text" && typeof p.text === "string")
        .map((p: any) => p.text)
        .join("\n")
        .trim();

      if (prompt) {
        state.prompt = prompt.substring(0, 500);
        debug("PROMPT_CAPTURED", {
          sessionID,
          messageID: input.messageID,
          agent: input.agent,
          model: input.model?.modelID,
          prompt: state.prompt,
        });
      }
    },

    // DEBUG: Hook into tool.execute.before to see tool args
    "tool.execute.before": async (input: any, output: any) => {
      debug("TOOL_BEFORE", {
        tool: input.tool,
        sessionID: input.sessionID,
        callID: input.callID,
        args: output.args,
      });
    },

    // tool.execute.after — rich metadata including structured diffs, diagnostics, file paths
    "tool.execute.after": async (input: any, output: any) => {
      debug("TOOL_AFTER", {
        tool: input.tool,
        sessionID: input.sessionID,
        callID: input.callID,
        args: input.args,
        title: output.title,
        output: output.output?.substring(0, 1000),
        metadata: output.metadata,
      });

      // Enrich the pending tool call with metadata from the after hook.
      // This data is FAR richer than what the event stream provides:
      // - metadata.filediff: { file, before, after, additions, deletions }
      // - metadata.diff: unified diff string
      // - metadata.diagnostics: LSP diagnostics per file
      // - metadata.exit: exit code for bash tools
      // - input.args: filePath, content, oldString, newString
      // - output.title: human-readable description
      if (input.sessionID && input.callID) {
        const state = ensure(input.sessionID);
        const pending = state.pendingTools.get(input.callID);
        if (pending) {
          pending.input = input.args;
          pending.output = output.output?.substring(0, 500);
          // Store the rich metadata for the after-tool hook payload
          (pending as any).title = output.title;
          (pending as any).filediff = output.metadata?.filediff;
          (pending as any).diff = output.metadata?.diff;
          (pending as any).diagnostics = output.metadata?.diagnostics;
          (pending as any).filePath = input.args?.filePath;
          (pending as any).exitCode = output.metadata?.exit;
        }
      }
    },

    "shell.env": async (_input: any, output: any) => {
      output.env.ATOMIC_AGENT = "opencode";
    },
  };
};
