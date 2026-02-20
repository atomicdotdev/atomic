/**
 * Atomic VCS Hooks Plugin — Compaction Handler Tests
 *
 * Unit tests for the compaction handler that reads provenance graphs
 * from disk and injects structured summaries into OpenCode's compaction
 * context.
 *
 * Tests cover:
 * - Reading and formatting provenance graph JSON
 * - Graceful handling of missing/corrupt graph files
 * - Summary content for various graph shapes
 * - Token budget (summary size)
 *
 * @module atomic/__tests__/compaction.test
 */

import { describe, test, expect, beforeEach } from "bun:test"
import { mkdirSync, writeFileSync, rmSync } from "fs"
import { join } from "path"
import { tmpdir } from "os"

import { createCompactionHandler } from "../handlers/compaction"
import type { CompactionOutput } from "../handlers/compaction"

// =============================================================================
// Helpers
// =============================================================================

let testDir: string
let counter = 0

function freshDir(): string {
  counter++
  const dir = join(tmpdir(), `atomic-compaction-test-${Date.now()}-${counter}`)
  mkdirSync(dir, { recursive: true })
  return dir
}

function makeGraphDir(directory: string, sessionID: string): string {
  const dir = join(directory, ".atomic", "sessions", sessionID)
  mkdirSync(dir, { recursive: true })
  return dir
}

function writeGraph(directory: string, sessionID: string, graph: object): void {
  const dir = makeGraphDir(directory, sessionID)
  writeFileSync(join(dir, "graph.json"), JSON.stringify(graph))
}

function makeOutput(): CompactionOutput {
  return { context: [] }
}

function makeLog() {
  const entries: Array<{ level: string; msg: string }> = []
  return {
    log: {
      info: (msg: string) => entries.push({ level: "info", msg }),
      warn: (msg: string) => entries.push({ level: "warn", msg }),
      debug: (msg: string) => entries.push({ level: "debug", msg }),
      error: (msg: string) => entries.push({ level: "error", msg }),
    } as any,
    entries,
  }
}

function makeGraph(nodes: object[], edges: object[] = []) {
  return {
    version: 1,
    session_id: "test-session",
    created_at: Date.now(),
    nodes,
    edges,
    stats: {
      goal_count: 0,
      exploration_count: 0,
      decision_count: 0,
      commitment_count: 0,
      verification_count: 0,
      human_gate_count: 0,
      error_count: 0,
      execution_count: 0,
      patch_proposal_count: 0,
      edge_count: 0,
    },
    counter: nodes.length,
  }
}

function goalNode(id: string, summary: string) {
  return { id, kind: "goal", timestamp: Date.now(), summary }
}

function explorationNode(id: string, summary: string) {
  return { id, kind: "exploration", timestamp: Date.now(), summary, tool_name: "read" }
}

function commitmentNode(id: string, summary: string) {
  return { id, kind: "commitment", timestamp: Date.now(), summary, tool_name: "edit" }
}

function verificationNode(id: string, summary: string) {
  return { id, kind: "verification", timestamp: Date.now(), summary, tool_name: "bash" }
}

function patchNode(id: string, summary: string) {
  return { id, kind: "patch_proposal", timestamp: Date.now(), summary, change_hash: "ABCD1234" }
}

function gateNode(id: string, summary: string, resolved: boolean) {
  return { id, kind: "human_gate", timestamp: Date.now(), summary, detail: { reason: summary, resolved } }
}

function errorNode(id: string, summary: string) {
  return { id, kind: "error", timestamp: Date.now(), summary, tool_name: "edit" }
}

function decisionNode(id: string, summary: string) {
  return { id, kind: "decision", timestamp: Date.now(), summary, classified: true, confidence: 0.9 }
}

// =============================================================================
// Cleanup
// =============================================================================

beforeEach(() => {
  testDir = freshDir()
})

// =============================================================================
// createCompactionHandler — basic behavior
// =============================================================================

describe("createCompactionHandler", () => {
  test("returns an async function", () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })
    expect(typeof handler).toBe("function")
  })

  test("injects summary when graph exists", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-1", makeGraph([
      goalNode("n-1", "Fix the auth bug"),
      explorationNode("n-2", "Read src/auth.rs"),
      commitmentNode("n-3", "Edit src/auth.rs"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-1" }, output)

    expect(output.context.length).toBe(1)
    expect(output.context[0]).toContain("## Session Provenance")
    expect(output.context[0]).toContain("Fix the auth bug")
  })

  test("does nothing when graph file does not exist", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    const output = makeOutput()
    await handler({ sessionID: "no-such-session" }, output)

    expect(output.context.length).toBe(0)
  })

  test("does nothing when graph is empty", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-empty", makeGraph([]))

    const output = makeOutput()
    await handler({ sessionID: "sess-empty" }, output)

    expect(output.context.length).toBe(0)
  })

  test("does nothing when graph JSON is corrupt", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    const dir = makeGraphDir(testDir, "sess-corrupt")
    writeFileSync(join(dir, "graph.json"), "not valid json {{{")

    const output = makeOutput()
    await handler({ sessionID: "sess-corrupt" }, output)

    expect(output.context.length).toBe(0)
  })

  test("does not set output.prompt", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-prompt", makeGraph([
      goalNode("n-1", "Fix something"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-prompt" }, output)

    expect(output.prompt).toBeUndefined()
  })

  test("preserves existing context entries", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-preserve", makeGraph([
      goalNode("n-1", "Fix bug"),
      commitmentNode("n-2", "Edit file"),
    ]))

    const output: CompactionOutput = { context: ["existing context from another plugin"] }
    await handler({ sessionID: "sess-preserve" }, output)

    expect(output.context.length).toBe(2)
    expect(output.context[0]).toBe("existing context from another plugin")
    expect(output.context[1]).toContain("## Session Provenance")
  })
})

// =============================================================================
// Summary content — Goals
// =============================================================================

describe("compaction summary — goals", () => {
  test("includes goals section", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-goals", makeGraph([
      goalNode("n-1", "Fix the auth bug in login.rs"),
      goalNode("n-2", "Add tests for the fix"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-goals" }, output)

    expect(output.context[0]).toContain("### Goals")
    expect(output.context[0]).toContain("Fix the auth bug in login.rs")
    expect(output.context[0]).toContain("Add tests for the fix")
  })
})

// =============================================================================
// Summary content — Changes Made
// =============================================================================

describe("compaction summary — changes", () => {
  test("includes changes section for commitments", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-changes", makeGraph([
      goalNode("n-1", "Fix bug"),
      commitmentNode("n-2", "Edit src/auth/login.rs"),
      commitmentNode("n-3", "Edit src/auth/jwt.rs"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-changes" }, output)

    expect(output.context[0]).toContain("### Changes Made")
    expect(output.context[0]).toContain("Edit src/auth/login.rs")
    expect(output.context[0]).toContain("Edit src/auth/jwt.rs")
  })
})

// =============================================================================
// Summary content — Verifications
// =============================================================================

describe("compaction summary — verifications", () => {
  test("includes verifications section", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-verify", makeGraph([
      goalNode("n-1", "Fix bug"),
      commitmentNode("n-2", "Edit auth.rs"),
      verificationNode("n-3", "cargo test --lib (passed)"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-verify" }, output)

    expect(output.context[0]).toContain("### Verifications")
    expect(output.context[0]).toContain("cargo test --lib (passed)")
  })
})

// =============================================================================
// Summary content — Patches
// =============================================================================

describe("compaction summary — patches", () => {
  test("includes recorded changes section", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-patch", makeGraph([
      goalNode("n-1", "Fix bug"),
      patchNode("n-2", "Change ABCD1234: src/auth.rs"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-patch" }, output)

    expect(output.context[0]).toContain("### Recorded Changes")
    expect(output.context[0]).toContain("Change ABCD1234")
  })
})

// =============================================================================
// Summary content — Human Gates
// =============================================================================

describe("compaction summary — human gates", () => {
  test("shows pending gate", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-gate-pending", makeGraph([
      goalNode("n-1", "Refactor auth"),
      gateNode("n-2", "Delete old token storage?", false),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-gate-pending" }, output)

    expect(output.context[0]).toContain("### Human Gates")
    expect(output.context[0]).toContain("pending")
  })

  test("shows resolved gate", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-gate-resolved", makeGraph([
      goalNode("n-1", "Refactor auth"),
      gateNode("n-2", "Delete old token storage?", true),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-gate-resolved" }, output)

    expect(output.context[0]).toContain("resolved")
  })
})

// =============================================================================
// Summary content — Errors
// =============================================================================

describe("compaction summary — errors", () => {
  test("includes errors section", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-errors", makeGraph([
      goalNode("n-1", "Fix bug"),
      errorNode("n-2", "edit failed: File not found"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-errors" }, output)

    expect(output.context[0]).toContain("### Errors Encountered")
    expect(output.context[0]).toContain("edit failed")
  })
})

// =============================================================================
// Summary content — Decisions (Phase 3)
// =============================================================================

describe("compaction summary — decisions", () => {
  test("includes decisions when present", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-decisions", makeGraph([
      goalNode("n-1", "Fix auth"),
      decisionNode("n-2", "Explored auth module → chose JWT timezone fix"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-decisions" }, output)

    expect(output.context[0]).toContain("### Decisions")
    expect(output.context[0]).toContain("Explored auth module")
  })
})

// =============================================================================
// Summary — full session flow
// =============================================================================

describe("compaction summary — full session", () => {
  test("formats a realistic session with all node types", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-full", makeGraph([
      goalNode("n-1", "Fix the auth bug in login.rs"),
      explorationNode("n-2", "Read src/auth/login.rs"),
      explorationNode("n-3", "Read src/auth/jwt.rs"),
      explorationNode("n-4", "Search fn validate_token"),
      commitmentNode("n-5", "Edit src/auth/login.rs"),
      verificationNode("n-6", "cargo test --lib (passed)"),
      patchNode("n-7", "Change ABCD1234: src/auth/login.rs"),
      goalNode("n-8", "Add tests for the token fix"),
      commitmentNode("n-9", "Create tests/auth_test.rs"),
      verificationNode("n-10", "cargo test tests::auth (passed)"),
      patchNode("n-11", "Change EFGH5678: tests/auth_test.rs"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-full" }, output)

    const summary = output.context[0]
    expect(summary).toBeDefined()

    // Has all sections
    expect(summary).toContain("### Goals")
    expect(summary).toContain("### Changes Made")
    expect(summary).toContain("### Verifications")
    expect(summary).toContain("### Recorded Changes")

    // Has specific content
    expect(summary).toContain("Fix the auth bug in login.rs")
    expect(summary).toContain("Add tests for the token fix")
    expect(summary).toContain("Edit src/auth/login.rs")
    expect(summary).toContain("cargo test")
    expect(summary).toContain("ABCD1234")
    expect(summary).toContain("EFGH5678")
  })

  test("summary is under 2000 characters for a 20-node graph", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    const nodes = [
      goalNode("n-1", "Fix the authentication bug in the login module"),
      explorationNode("n-2", "Read src/auth/login.rs"),
      explorationNode("n-3", "Read src/auth/jwt.rs"),
      explorationNode("n-4", "Read src/auth/middleware.rs"),
      explorationNode("n-5", "Search validate_token across codebase"),
      commitmentNode("n-6", "Edit src/auth/login.rs"),
      verificationNode("n-7", "cargo test --lib (passed)"),
      patchNode("n-8", "Change ABCD1234: src/auth/login.rs"),
      goalNode("n-9", "Add comprehensive tests for the token validation fix"),
      explorationNode("n-10", "Read tests/auth_test.rs"),
      commitmentNode("n-11", "Create tests/auth_test.rs"),
      commitmentNode("n-12", "Edit tests/auth_test.rs"),
      verificationNode("n-13", "cargo test tests::auth (passed)"),
      patchNode("n-14", "Change EFGH5678: 2 files"),
      goalNode("n-15", "Fix the refresh token endpoint with the same timezone bug"),
      explorationNode("n-16", "Read src/auth/refresh.rs"),
      commitmentNode("n-17", "Edit src/auth/refresh.rs"),
      verificationNode("n-18", "cargo test --lib (passed)"),
      patchNode("n-19", "Change IJKL9012: src/auth/refresh.rs"),
      errorNode("n-20", "edit failed: src/auth/legacy.rs not found"),
    ]

    writeGraph(testDir, "sess-budget", makeGraph(nodes))

    const output = makeOutput()
    await handler({ sessionID: "sess-budget" }, output)

    const summary = output.context[0]
    expect(summary).toBeDefined()
    expect(summary.length).toBeLessThan(2000)
  })
})

// =============================================================================
// Edge cases
// =============================================================================

describe("compaction handler — edge cases", () => {
  test("handles graph with only explorations (no goals)", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-no-goal", makeGraph([
      explorationNode("n-1", "Read src/main.rs"),
      explorationNode("n-2", "Read src/lib.rs"),
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-no-goal" }, output)

    // Should still produce something — explorations aren't shown in summary
    // but the graph header mentions node count
    // Actually explorations don't get their own section in the summary,
    // so the summary will just be the header, which is filtered out as empty.
    // This is correct — no actionable information to inject.
    expect(output.context.length).toBe(0)
  })

  test("handles graph with extra unknown fields gracefully", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    const graph = {
      ...makeGraph([goalNode("n-1", "Fix bug"), commitmentNode("n-2", "Edit file")]),
      unknown_field: "should be ignored",
      future_version_data: { nested: true },
    }
    writeGraph(testDir, "sess-extra", graph)

    const output = makeOutput()
    await handler({ sessionID: "sess-extra" }, output)

    expect(output.context.length).toBe(1)
    expect(output.context[0]).toContain("Fix bug")
  })

  test("handles empty .atomic directory", async () => {
    const { log } = makeLog()
    mkdirSync(join(testDir, ".atomic"), { recursive: true })
    const handler = createCompactionHandler({ log, directory: testDir })

    const output = makeOutput()
    await handler({ sessionID: "nonexistent" }, output)

    expect(output.context.length).toBe(0)
  })

  test("handles graph with nodes but no meaningful sections", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    // A graph with only execution nodes — no goals, commits, etc.
    writeGraph(testDir, "sess-exec-only", makeGraph([
      { id: "n-1", kind: "execution", timestamp: Date.now(), summary: "npm install express" },
    ]))

    const output = makeOutput()
    await handler({ sessionID: "sess-exec-only" }, output)

    // Execution nodes aren't shown in summary sections, so nothing to inject
    expect(output.context.length).toBe(0)
  })

  test("handles concurrent calls gracefully", async () => {
    const { log } = makeLog()
    const handler = createCompactionHandler({ log, directory: testDir })

    writeGraph(testDir, "sess-concurrent", makeGraph([
      goalNode("n-1", "Fix bug"),
      commitmentNode("n-2", "Edit file"),
    ]))

    // Fire multiple compaction calls simultaneously
    const outputs = Array.from({ length: 5 }, () => makeOutput())
    await Promise.all(outputs.map((output) => handler({ sessionID: "sess-concurrent" }, output)))

    // All should have gotten the same summary
    for (const output of outputs) {
      expect(output.context.length).toBe(1)
      expect(output.context[0]).toContain("Fix bug")
    }
  })
})
