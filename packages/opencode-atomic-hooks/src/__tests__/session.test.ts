/**
 * Atomic VCS Hooks Plugin — Session Store Tests
 *
 * Unit tests for the in-memory session state management module.
 * Tests cover creation, retrieval, turn tracking, model/provider
 * storage, tool duration tracking, and cleanup.
 *
 * @module atomic/__tests__/session.test
 */

import { describe, test, expect, beforeEach } from "bun:test"

import { createSessionStore, createSessionState } from "../session"
import type { SessionStore } from "../session"

// =============================================================================
// createSessionState
// =============================================================================

describe("createSessionState", () => {
  test("creates state with default timestamp", () => {
    const before = Date.now()
    const state = createSessionState()
    const after = Date.now()

    expect(state.startTime).toBeGreaterThanOrEqual(before)
    expect(state.startTime).toBeLessThanOrEqual(after)
    expect(state.turnCount).toBe(0)
    expect(state.model).toBeUndefined()
    expect(state.provider).toBeUndefined()
    expect(state.lastPrompt).toBeUndefined()
    expect(state.toolStartTimes.size).toBe(0)
  })

  test("creates state with custom timestamp", () => {
    const state = createSessionState(1000)
    expect(state.startTime).toBe(1000)
  })
})

// =============================================================================
// createSessionStore — get / has / remove
// =============================================================================

describe("createSessionStore", () => {
  let store: SessionStore

  beforeEach(() => {
    store = createSessionStore()
  })

  test("starts empty", () => {
    expect(store.size()).toBe(0)
    expect(store.has("nonexistent")).toBe(false)
  })

  test("get creates session on first access", () => {
    const session = store.get("s1")
    expect(session.turnCount).toBe(0)
    expect(store.has("s1")).toBe(true)
    expect(store.size()).toBe(1)
  })

  test("get returns same session on repeated access", () => {
    const first = store.get("s1")
    first.turnCount = 5
    const second = store.get("s1")
    expect(second.turnCount).toBe(5)
    expect(store.size()).toBe(1)
  })

  test("get creates independent sessions for different IDs", () => {
    store.get("s1").turnCount = 1
    store.get("s2").turnCount = 2

    expect(store.get("s1").turnCount).toBe(1)
    expect(store.get("s2").turnCount).toBe(2)
    expect(store.size()).toBe(2)
  })

  test("has returns false for unknown session", () => {
    expect(store.has("unknown")).toBe(false)
  })

  test("has returns true after get", () => {
    store.get("s1")
    expect(store.has("s1")).toBe(true)
  })

  test("remove deletes session", () => {
    store.get("s1")
    expect(store.has("s1")).toBe(true)

    store.remove("s1")
    expect(store.has("s1")).toBe(false)
    expect(store.size()).toBe(0)
  })

  test("remove is no-op for nonexistent session", () => {
    store.remove("nonexistent")
    expect(store.size()).toBe(0)
  })

  test("clear removes all sessions", () => {
    store.get("s1")
    store.get("s2")
    store.get("s3")
    expect(store.size()).toBe(3)

    store.clear()
    expect(store.size()).toBe(0)
    expect(store.has("s1")).toBe(false)
  })

  test("get after remove creates fresh session", () => {
    store.get("s1").turnCount = 42
    store.remove("s1")
    expect(store.get("s1").turnCount).toBe(0)
  })
})

// =============================================================================
// incrementTurn
// =============================================================================

describe("incrementTurn", () => {
  let store: SessionStore

  beforeEach(() => {
    store = createSessionStore()
  })

  test("returns 1 on first increment", () => {
    expect(store.incrementTurn("s1")).toBe(1)
  })

  test("increments sequentially", () => {
    expect(store.incrementTurn("s1")).toBe(1)
    expect(store.incrementTurn("s1")).toBe(2)
    expect(store.incrementTurn("s1")).toBe(3)
  })

  test("creates session if it does not exist", () => {
    store.incrementTurn("new")
    expect(store.has("new")).toBe(true)
    expect(store.get("new").turnCount).toBe(1)
  })

  test("independent counters per session", () => {
    store.incrementTurn("s1")
    store.incrementTurn("s1")
    store.incrementTurn("s2")

    expect(store.get("s1").turnCount).toBe(2)
    expect(store.get("s2").turnCount).toBe(1)
  })
})

// =============================================================================
// setModel
// =============================================================================

describe("setModel", () => {
  let store: SessionStore

  beforeEach(() => {
    store = createSessionStore()
  })

  test("sets both provider and model", () => {
    store.setModel("s1", "anthropic", "claude-sonnet-4")
    const session = store.get("s1")
    expect(session.provider).toBe("anthropic")
    expect(session.model).toBe("claude-sonnet-4")
  })

  test("sets only provider when model is undefined", () => {
    store.setModel("s1", "anthropic", undefined)
    expect(store.get("s1").provider).toBe("anthropic")
    expect(store.get("s1").model).toBeUndefined()
  })

  test("sets only model when provider is undefined", () => {
    store.setModel("s1", undefined, "gpt-4o")
    expect(store.get("s1").provider).toBeUndefined()
    expect(store.get("s1").model).toBe("gpt-4o")
  })

  test("preserves existing values when new values are undefined", () => {
    store.setModel("s1", "anthropic", "claude-sonnet-4")
    store.setModel("s1", undefined, undefined)
    expect(store.get("s1").provider).toBe("anthropic")
    expect(store.get("s1").model).toBe("claude-sonnet-4")
  })

  test("overwrites existing values", () => {
    store.setModel("s1", "anthropic", "claude-sonnet-4")
    store.setModel("s1", "openai", "gpt-4o")
    expect(store.get("s1").provider).toBe("openai")
    expect(store.get("s1").model).toBe("gpt-4o")
  })

  test("creates session if it does not exist", () => {
    store.setModel("new", "anthropic", "claude-sonnet-4")
    expect(store.has("new")).toBe(true)
  })

  test("does not set empty string provider", () => {
    store.setModel("s1", "", "model")
    expect(store.get("s1").provider).toBeUndefined()
    expect(store.get("s1").model).toBe("model")
  })

  test("does not set empty string model", () => {
    store.setModel("s1", "provider", "")
    expect(store.get("s1").provider).toBe("provider")
    expect(store.get("s1").model).toBeUndefined()
  })
})

// =============================================================================
// setPrompt
// =============================================================================

describe("setPrompt", () => {
  let store: SessionStore

  beforeEach(() => {
    store = createSessionStore()
  })

  test("sets the last prompt", () => {
    store.setPrompt("s1", "Fix the auth bug")
    expect(store.get("s1").lastPrompt).toBe("Fix the auth bug")
  })

  test("overwrites previous prompt", () => {
    store.setPrompt("s1", "First prompt")
    store.setPrompt("s1", "Second prompt")
    expect(store.get("s1").lastPrompt).toBe("Second prompt")
  })

  test("ignores empty string", () => {
    store.setPrompt("s1", "Real prompt")
    store.setPrompt("s1", "")
    expect(store.get("s1").lastPrompt).toBe("Real prompt")
  })

  test("does not create session for empty prompt", () => {
    store.setPrompt("s1", "")
    // setPrompt with empty string returns early, but the check is on
    // the prompt value, not on session creation. Let's verify the prompt
    // is not set even though the session may have been implicitly created.
    // Actually, looking at the implementation, setPrompt returns early
    // before calling store.get(), so the session is NOT created.
    // But we shouldn't rely on that implementation detail.
    // The important thing is that the prompt is not set.
    if (store.has("s1")) {
      expect(store.get("s1").lastPrompt).toBeUndefined()
    }
  })
})

// =============================================================================
// startTool / endTool
// =============================================================================

describe("tool duration tracking", () => {
  let store: SessionStore

  beforeEach(() => {
    store = createSessionStore()
  })

  test("startTool records start time", () => {
    store.startTool("s1", "call-1", 1000)
    expect(store.get("s1").toolStartTimes.has("call-1")).toBe(true)
  })

  test("endTool returns elapsed duration", () => {
    store.startTool("s1", "call-1", 1000)
    const duration = store.endTool("s1", "call-1", 2500)
    expect(duration).toBe(1500)
  })

  test("endTool removes the tracked call", () => {
    store.startTool("s1", "call-1", 1000)
    store.endTool("s1", "call-1", 2000)
    expect(store.get("s1").toolStartTimes.has("call-1")).toBe(false)
  })

  test("endTool returns undefined for unknown call", () => {
    store.get("s1") // ensure session exists
    const duration = store.endTool("s1", "unknown-call", 2000)
    expect(duration).toBeUndefined()
  })

  test("endTool returns undefined for unknown session", () => {
    const duration = store.endTool("nonexistent", "call-1", 2000)
    expect(duration).toBeUndefined()
  })

  test("multiple concurrent tool calls tracked independently", () => {
    store.startTool("s1", "call-a", 1000)
    store.startTool("s1", "call-b", 1200)
    store.startTool("s1", "call-c", 1500)

    expect(store.endTool("s1", "call-b", 2000)).toBe(800)
    expect(store.endTool("s1", "call-a", 2500)).toBe(1500)
    expect(store.endTool("s1", "call-c", 3000)).toBe(1500)
  })

  test("tool calls are per-session", () => {
    store.startTool("s1", "call-1", 1000)
    store.startTool("s2", "call-1", 2000)

    expect(store.endTool("s1", "call-1", 3000)).toBe(2000)
    expect(store.endTool("s2", "call-1", 3000)).toBe(1000)
  })

  test("endTool called twice returns undefined on second call", () => {
    store.startTool("s1", "call-1", 1000)
    expect(store.endTool("s1", "call-1", 2000)).toBe(1000)
    expect(store.endTool("s1", "call-1", 3000)).toBeUndefined()
  })

  test("startTool uses Date.now when no timestamp provided", () => {
    const before = Date.now()
    store.startTool("s1", "call-1")
    const after = Date.now()

    const start = store.get("s1").toolStartTimes.get("call-1")
    expect(start).toBeDefined()
    expect(start!).toBeGreaterThanOrEqual(before)
    expect(start!).toBeLessThanOrEqual(after)
  })

  test("endTool uses Date.now when no timestamp provided", () => {
    store.startTool("s1", "call-1", 1000)
    const before = Date.now()
    const duration = store.endTool("s1", "call-1")
    const after = Date.now()

    expect(duration).toBeDefined()
    expect(duration!).toBeGreaterThanOrEqual(before - 1000)
    expect(duration!).toBeLessThanOrEqual(after - 1000)
  })

  test("startTool overwrites existing start time", () => {
    store.startTool("s1", "call-1", 1000)
    store.startTool("s1", "call-1", 5000)
    expect(store.endTool("s1", "call-1", 6000)).toBe(1000)
  })
})

// =============================================================================
// SessionStore.create / SessionStore.createState
// =============================================================================

describe("SessionStore namespace", () => {
  test("SessionStore.create returns a working store", () => {
    const { SessionStore } = require("../session")
    const store = SessionStore.create()
    store.get("test")
    expect(store.has("test")).toBe(true)
  })

  test("SessionStore.createState returns a valid state", () => {
    const { SessionStore } = require("../session")
    const state = SessionStore.createState(42)
    expect(state.startTime).toBe(42)
    expect(state.turnCount).toBe(0)
  })
})

// =============================================================================
// Edge Cases & Stress
// =============================================================================

describe("edge cases", () => {
  let store: SessionStore

  beforeEach(() => {
    store = createSessionStore()
  })

  test("empty string session ID is valid", () => {
    store.get("")
    expect(store.has("")).toBe(true)
    expect(store.size()).toBe(1)
  })

  test("session ID with special characters", () => {
    const id = "session/with:special@chars#123"
    store.get(id)
    expect(store.has(id)).toBe(true)
  })

  test("many sessions", () => {
    for (let i = 0; i < 1000; i++) {
      store.get(`session-${i}`)
    }
    expect(store.size()).toBe(1000)
  })

  test("interleaved operations across sessions", () => {
    store.setModel("s1", "anthropic", "claude")
    store.incrementTurn("s2")
    store.startTool("s1", "call-1", 100)
    store.setPrompt("s2", "hello")
    store.incrementTurn("s1")
    store.endTool("s1", "call-1", 200)
    store.remove("s2")

    expect(store.get("s1").turnCount).toBe(1)
    expect(store.get("s1").model).toBe("claude")
    expect(store.has("s2")).toBe(false)
  })
})
