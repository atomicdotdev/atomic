/**
 * Atomic VCS Hooks Plugin — Session Compaction Handler
 *
 * Handles the `experimental.session.compacting` plugin hook, which fires
 * when OpenCode compacts the conversation to fit within the context window.
 *
 * Without this handler, all tool call history and reasoning context is lost
 * at compaction — the LLM starts fresh with a generic summary. This handler
 * reads the provenance graph built by the Rust-side `TurnOrchestrator` from
 * disk and injects a structured summary into the compacted context.
 *
 * The result: the LLM retains structural knowledge of what was explored,
 * decided, committed, and verified — even after compaction. Multi-hour
 * sessions maintain coherent decision histories.
 *
 * ## Data Flow
 *
 * ```
 * OpenCode triggers compaction
 *       │
 *       ▼
 * experimental.session.compacting hook fires
 *       │
 *       ├── Read .atomic/sessions/{sessionID}/graph.json
 *       │     (written by Rust-side TurnOrchestrator on every hook)
 *       │
 *       ├── Format a token-efficient structured summary
 *       │     (goals, changes, verifications, human gates)
 *       │
 *       └── Push summary into output.context[]
 *             (survives compaction, injected into LLM context)
 * ```
 *
 * ## Design
 *
 * This handler is intentionally thin — the provenance graph is built and
 * maintained entirely on the Rust side. This handler just reads the JSON
 * file from disk and formats it. No graph accumulation, no classification,
 * no state management. Consistent with the plugin's "thin pipe" design.
 *
 * All errors are swallowed — if the graph file doesn't exist (session
 * hasn't recorded any tool calls yet) or can't be parsed, the handler
 * returns silently. Compaction must never be blocked by provenance.
 *
 * @module atomic/handlers/compaction
 */

import { readFile } from "fs/promises"
import { join } from "path"

import type { Logger } from "../log"

// =============================================================================
// Types
// =============================================================================

/**
 * Dependencies injected into the compaction handler.
 */
export interface CompactionHandlerDeps {
  /** Structured logger */
  log: Logger
  /** Project working directory (where .atomic/ lives) */
  directory: string
}

/**
 * The `input` parameter shape for `experimental.session.compacting`.
 */
export interface CompactionInput {
  sessionID: string
}

/**
 * The `output` parameter shape for `experimental.session.compacting`.
 */
export interface CompactionOutput {
  /** Additional context strings appended to the compaction prompt */
  context: string[]
  /** If set, replaces the default compaction prompt entirely */
  prompt?: string
}

// =============================================================================
// Graph JSON types (mirrors Rust SerializedGraph)
// =============================================================================

interface GraphNode {
  id: string
  kind: string
  timestamp: number
  summary: string
  detail?: Record<string, unknown>
  change_hash?: string
  tool_name?: string
  tool_call_id?: string
  duration_ms?: number
  classified?: boolean
  confidence?: number
  consolidated_from?: string[]
}

interface SerializedGraph {
  version: number
  session_id: string
  created_at: number
  nodes: GraphNode[]
  edges: Array<{ from: string; to: string; kind: string }>
  stats: Record<string, number>
  counter: number
}

// =============================================================================
// Handler Factory
// =============================================================================

/**
 * Create a compaction handler that injects the provenance graph summary
 * into the compacted context.
 *
 * The handler reads the graph from `.atomic/sessions/{sessionID}/graph.json`,
 * formats a structured summary, and pushes it into `output.context[]`.
 *
 * @param deps - Injected dependencies
 * @returns An async function matching the `Hooks["experimental.session.compacting"]` signature
 *
 * @example
 * ```ts
 * const handler = createCompactionHandler({
 *   log,
 *   directory: ctx.directory,
 * })
 *
 * // In the plugin hooks object:
 * return { "experimental.session.compacting": handler }
 * ```
 */
export function createCompactionHandler(deps: CompactionHandlerDeps) {
  const { log, directory } = deps

  return async (input: CompactionInput, output: CompactionOutput): Promise<void> => {
    const graphPath = join(directory, ".atomic", "sessions", input.sessionID, "graph.json")

    try {
      const raw = await readFile(graphPath, "utf-8")
      const graph: SerializedGraph = JSON.parse(raw)
      const summary = formatCompactionSummary(graph)

      if (!summary) return

      output.context.push(summary)

      log.debug("Injected provenance summary into compaction context", {
        sessionID: input.sessionID,
        nodes: graph.nodes.length,
        edges: graph.edges.length,
        summaryLength: summary.length,
      })
    } catch {
      // Graph doesn't exist yet or can't be read — that's fine.
      // Early in a session there may be no tool calls recorded yet.
      // This is expected and not worth logging above debug level.
      log.debug("No provenance graph available for compaction", {
        sessionID: input.sessionID,
        path: graphPath,
      })
    }
  }
}

// =============================================================================
// Summary Formatting
// =============================================================================

/**
 * Format a provenance graph into a token-efficient structured summary
 * suitable for injection into the LLM's compaction context.
 *
 * The summary is optimized for the LLM to understand what has happened
 * in the session so far — what goals were set, what was explored, what
 * was changed, what was verified, and what is still pending.
 *
 * Targets < 500 tokens for a typical 20-node session.
 *
 * @param graph - The deserialized provenance graph
 * @returns The formatted summary, or `null` if the graph is empty
 */
function formatCompactionSummary(graph: SerializedGraph): string | null {
  if (!graph.nodes || graph.nodes.length === 0) return null

  const goals = graph.nodes.filter((n) => n.kind === "goal")
  const decisions = graph.nodes.filter((n) => n.kind === "decision")
  const commitments = graph.nodes.filter((n) => n.kind === "commitment")
  const verifications = graph.nodes.filter((n) => n.kind === "verification")
  const patches = graph.nodes.filter((n) => n.kind === "patch_proposal")
  const gates = graph.nodes.filter((n) => n.kind === "human_gate")
  const errors = graph.nodes.filter((n) => n.kind === "error")

  const lines: string[] = [
    `## Session Provenance (${graph.nodes.length} nodes, ${graph.edges.length} edges)`,
    "",
  ]

  // Goals — what the human asked for
  if (goals.length > 0) {
    lines.push("### Goals")
    for (const g of goals) lines.push(`- ${g.summary}`)
    lines.push("")
  }

  // Decisions — consolidated strategy choices (Phase 3, may not exist yet)
  if (decisions.length > 0) {
    lines.push("### Decisions")
    for (const d of decisions) lines.push(`- ${d.summary}`)
    lines.push("")
  }

  // Changes made — file modifications
  if (commitments.length > 0) {
    lines.push("### Changes Made")
    for (const c of commitments) lines.push(`- ${c.summary}`)
    lines.push("")
  }

  // Verifications — test/lint/build results
  if (verifications.length > 0) {
    lines.push("### Verifications")
    for (const v of verifications) lines.push(`- ${v.summary}`)
    lines.push("")
  }

  // Recorded changes — patch proposals with change hashes
  if (patches.length > 0) {
    lines.push("### Recorded Changes")
    for (const p of patches) lines.push(`- ${p.summary}`)
    lines.push("")
  }

  // Human gates — pending or resolved permission requests
  if (gates.length > 0) {
    lines.push("### Human Gates")
    for (const g of gates) {
      const resolved = g.detail?.resolved === true
      lines.push(`- ${g.summary} (${resolved ? "resolved" : "pending"})`)
    }
    lines.push("")
  }

  // Errors — tool failures worth remembering
  if (errors.length > 0) {
    lines.push("### Errors Encountered")
    for (const e of errors) lines.push(`- ${e.summary}`)
    lines.push("")
  }

  // Trim trailing blank line
  while (lines.length > 0 && lines[lines.length - 1] === "") lines.pop()

  // Don't inject an empty summary (just the header)
  if (lines.length <= 2) return null

  return lines.join("\n")
}
