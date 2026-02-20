/**
 * Atomic VCS Hooks Plugin — Handler Helper Tests
 *
 * Unit tests for the pure helper functions exported from the handler
 * modules. These are the functions that don't require a shell or CLI
 * invocation — they can be tested in complete isolation.
 *
 * Covered functions:
 *
 * - `extractPrompt` (chat.ts) — Extract prompt text from message parts
 * - `isEditTool` (tool.ts) — Classify tools as file-modifying or read-only
 * - `truncateOutput` (tool.ts) — Truncate tool output to safe length
 * - `createShellHandler` (shell.ts) — Inject env vars into shell commands
 * - `defaultShellConfig` (shell.ts) — Build default shell config
 *
 * @module atomic/__tests__/handlers.test
 */

import { describe, test, expect } from "bun:test"

import { extractPrompt } from "../handlers/chat"
import { isEditTool, truncateOutput } from "../handlers/tool"
import { createShellHandler, defaultShellConfig } from "../handlers/shell"
import { EDIT_TOOLS, MAX_TOOL_OUTPUT_LENGTH, AGENT_NAME, PLUGIN_VERSION } from "../constants"

// =============================================================================
// extractPrompt
// =============================================================================

describe("extractPrompt", () => {
  test("extracts text from a single text part", () => {
    const parts = [{ type: "text", text: "Fix the auth bug" }]
    expect(extractPrompt(parts)).toBe("Fix the auth bug")
  })

  test("concatenates multiple text parts with newlines", () => {
    const parts = [
      { type: "text", text: "First line" },
      { type: "text", text: "Second line" },
    ]
    expect(extractPrompt(parts)).toBe("First line\nSecond line")
  })

  test("ignores non-text parts", () => {
    const parts = [
      { type: "image", url: "https://example.com/img.png" },
      { type: "text", text: "Hello" },
      { type: "tool_result", tool_use_id: "abc" },
    ]
    expect(extractPrompt(parts)).toBe("Hello")
  })

  test("returns undefined for empty parts array", () => {
    expect(extractPrompt([])).toBeUndefined()
  })

  test("returns undefined when no text parts exist", () => {
    const parts = [
      { type: "image", url: "https://example.com/img.png" },
      { type: "tool_result", tool_use_id: "abc" },
    ]
    expect(extractPrompt(parts)).toBeUndefined()
  })

  test("returns undefined when all text parts are empty", () => {
    const parts = [
      { type: "text", text: "" },
      { type: "text", text: "   " },
    ]
    expect(extractPrompt(parts)).toBeUndefined()
  })

  test("trims leading and trailing whitespace", () => {
    const parts = [{ type: "text", text: "  hello world  " }]
    expect(extractPrompt(parts)).toBe("hello world")
  })

  test("handles text parts with missing text field", () => {
    const parts = [
      { type: "text" },
      { type: "text", text: "valid" },
    ]
    expect(extractPrompt(parts)).toBe("valid")
  })

  test("handles text parts where text is not a string", () => {
    const parts = [
      { type: "text", text: 42 },
      { type: "text", text: "valid" },
    ]
    expect(extractPrompt(parts as any)).toBe("valid")
  })

  test("preserves internal whitespace in prompt text", () => {
    const parts = [{ type: "text", text: "line 1\n\n  indented\n\nline 4" }]
    expect(extractPrompt(parts)).toBe("line 1\n\n  indented\n\nline 4")
  })

  test("handles unicode content", () => {
    const parts = [{ type: "text", text: "修复身份验证错误 🐛" }]
    expect(extractPrompt(parts)).toBe("修复身份验证错误 🐛")
  })

  test("handles very long prompts without truncation", () => {
    const longText = "a".repeat(100_000)
    const parts = [{ type: "text", text: longText }]
    expect(extractPrompt(parts)).toBe(longText)
  })
})

// =============================================================================
// isEditTool
// =============================================================================

describe("isEditTool", () => {
  test("returns true for known edit tools", () => {
    for (const tool of EDIT_TOOLS) {
      expect(isEditTool(tool)).toBe(true)
    }
  })

  test("returns true for 'edit'", () => {
    expect(isEditTool("edit")).toBe(true)
  })

  test("returns true for 'write'", () => {
    expect(isEditTool("write")).toBe(true)
  })

  test("returns true for 'multiedit'", () => {
    expect(isEditTool("multiedit")).toBe(true)
  })

  test("returns true for 'patch'", () => {
    expect(isEditTool("patch")).toBe(true)
  })

  test("returns true for 'bash'", () => {
    expect(isEditTool("bash")).toBe(true)
  })

  test("returns false for 'read'", () => {
    expect(isEditTool("read")).toBe(false)
  })

  test("returns false for 'grep'", () => {
    expect(isEditTool("grep")).toBe(false)
  })

  test("returns false for 'list_directory'", () => {
    expect(isEditTool("list_directory")).toBe(false)
  })

  test("returns false for 'find_path'", () => {
    expect(isEditTool("find_path")).toBe(false)
  })

  test("returns false for empty string", () => {
    expect(isEditTool("")).toBe(false)
  })

  test("is case-sensitive", () => {
    expect(isEditTool("Edit")).toBe(false)
    expect(isEditTool("BASH")).toBe(false)
    expect(isEditTool("Write")).toBe(false)
  })

  test("returns false for unknown tools", () => {
    expect(isEditTool("custom_tool")).toBe(false)
    expect(isEditTool("my-special-tool")).toBe(false)
  })
})

// =============================================================================
// truncateOutput
// =============================================================================

describe("truncateOutput", () => {
  test("returns empty object for undefined output", () => {
    expect(truncateOutput(undefined)).toEqual({})
  })

  test("returns empty object for empty string", () => {
    expect(truncateOutput("")).toEqual({})
  })

  test("preserves short output unchanged", () => {
    const result = truncateOutput("hello world")
    expect(result).toEqual({ tool_output: "hello world" })
  })

  test("preserves output at exactly the max length", () => {
    const output = "x".repeat(MAX_TOOL_OUTPUT_LENGTH)
    const result = truncateOutput(output)
    expect(result.tool_output).toBe(output)
    expect(result.tool_output!.length).toBe(MAX_TOOL_OUTPUT_LENGTH)
  })

  test("truncates output exceeding max length", () => {
    const output = "x".repeat(MAX_TOOL_OUTPUT_LENGTH + 100)
    const result = truncateOutput(output)
    expect(result.tool_output!.length).toBe(MAX_TOOL_OUTPUT_LENGTH)
  })

  test("truncates very large output", () => {
    const output = "y".repeat(1_000_000)
    const result = truncateOutput(output)
    expect(result.tool_output!.length).toBe(MAX_TOOL_OUTPUT_LENGTH)
    expect(result.tool_output).toBe("y".repeat(MAX_TOOL_OUTPUT_LENGTH))
  })

  test("preserves content at the truncation boundary", () => {
    const prefix = "KEEP"
    const suffix = "DISCARD"
    const padding = "x".repeat(MAX_TOOL_OUTPUT_LENGTH - prefix.length)
    const output = prefix + padding + suffix
    const result = truncateOutput(output)

    expect(result.tool_output!.startsWith("KEEP")).toBe(true)
    expect(result.tool_output!.includes("DISCARD")).toBe(false)
  })

  test("handles single character output", () => {
    expect(truncateOutput("a")).toEqual({ tool_output: "a" })
  })

  test("handles whitespace-only output", () => {
    expect(truncateOutput("   ")).toEqual({ tool_output: "   " })
  })

  test("handles output with newlines", () => {
    const output = "line1\nline2\nline3"
    expect(truncateOutput(output)).toEqual({ tool_output: output })
  })

  test("handles unicode output", () => {
    const emoji = "🎉".repeat(10)
    const result = truncateOutput(emoji)
    expect(result.tool_output).toBeDefined()
    // Note: substring works on UTF-16 code units, so emoji may be split.
    // The important thing is we don't crash and the length is bounded.
    expect(result.tool_output!.length).toBeLessThanOrEqual(MAX_TOOL_OUTPUT_LENGTH)
  })
})

// =============================================================================
// createShellHandler
// =============================================================================

describe("createShellHandler", () => {
  test("injects ATOMIC_AGENT with default value", async () => {
    const handler = createShellHandler()
    const output = { env: {} as Record<string, string> }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.ATOMIC_AGENT).toBe(AGENT_NAME)
  })

  test("injects ATOMIC_AGENT_VERSION with default value", async () => {
    const handler = createShellHandler()
    const output = { env: {} as Record<string, string> }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.ATOMIC_AGENT_VERSION).toBe(PLUGIN_VERSION)
  })

  test("uses custom agent name", async () => {
    const handler = createShellHandler({ agentName: "custom-agent" })
    const output = { env: {} as Record<string, string> }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.ATOMIC_AGENT).toBe("custom-agent")
  })

  test("uses custom version", async () => {
    const handler = createShellHandler({ version: "2.0.0-beta" })
    const output = { env: {} as Record<string, string> }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.ATOMIC_AGENT_VERSION).toBe("2.0.0-beta")
  })

  test("uses both custom values together", async () => {
    const handler = createShellHandler({
      agentName: "test-agent",
      version: "0.0.1-test",
    })
    const output = { env: {} as Record<string, string> }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.ATOMIC_AGENT).toBe("test-agent")
    expect(output.env.ATOMIC_AGENT_VERSION).toBe("0.0.1-test")
  })

  test("preserves existing env vars", async () => {
    const handler = createShellHandler()
    const output = {
      env: {
        PATH: "/usr/bin",
        HOME: "/home/user",
      } as Record<string, string>,
    }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.PATH).toBe("/usr/bin")
    expect(output.env.HOME).toBe("/home/user")
    expect(output.env.ATOMIC_AGENT).toBe(AGENT_NAME)
  })

  test("overwrites existing ATOMIC_AGENT if present", async () => {
    const handler = createShellHandler()
    const output = {
      env: { ATOMIC_AGENT: "old-value" } as Record<string, string>,
    }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.ATOMIC_AGENT).toBe(AGENT_NAME)
  })

  test("partial config uses defaults for missing fields", async () => {
    const handler = createShellHandler({ agentName: "custom" })
    const output = { env: {} as Record<string, string> }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.ATOMIC_AGENT).toBe("custom")
    expect(output.env.ATOMIC_AGENT_VERSION).toBe(PLUGIN_VERSION)
  })

  test("empty config uses all defaults", async () => {
    const handler = createShellHandler({})
    const output = { env: {} as Record<string, string> }
    await handler({ cwd: "/tmp" }, output)

    expect(output.env.ATOMIC_AGENT).toBe(AGENT_NAME)
    expect(output.env.ATOMIC_AGENT_VERSION).toBe(PLUGIN_VERSION)
  })
})

// =============================================================================
// defaultShellConfig
// =============================================================================

describe("defaultShellConfig", () => {
  test("returns expected agent name", () => {
    const config = defaultShellConfig()
    expect(config.agentName).toBe(AGENT_NAME)
  })

  test("returns expected version", () => {
    const config = defaultShellConfig()
    expect(config.version).toBe(PLUGIN_VERSION)
  })

  test("returns a fresh object each time", () => {
    const a = defaultShellConfig()
    const b = defaultShellConfig()
    expect(a).toEqual(b)
    expect(a).not.toBe(b)
  })
})

// =============================================================================
// Constants validation
// =============================================================================

describe("constants", () => {
  test("EDIT_TOOLS contains at least the core edit tools", () => {
    expect(EDIT_TOOLS.has("edit")).toBe(true)
    expect(EDIT_TOOLS.has("write")).toBe(true)
    expect(EDIT_TOOLS.has("bash")).toBe(true)
  })

  test("MAX_TOOL_OUTPUT_LENGTH is a reasonable value", () => {
    expect(MAX_TOOL_OUTPUT_LENGTH).toBeGreaterThan(0)
    expect(MAX_TOOL_OUTPUT_LENGTH).toBeLessThanOrEqual(10_000)
  })

  test("AGENT_NAME is 'opencode'", () => {
    expect(AGENT_NAME).toBe("opencode")
  })

  test("PLUGIN_VERSION matches semver pattern", () => {
    expect(PLUGIN_VERSION).toMatch(/^\d+\.\d+\.\d+/)
  })
})

// =============================================================================
// Integration: handler factories return functions
// =============================================================================

describe("handler factory shapes", () => {
  test("createShellHandler returns an async function", () => {
    const handler = createShellHandler()
    expect(typeof handler).toBe("function")
  })

  test("createShellHandler result accepts correct arguments", async () => {
    const handler = createShellHandler()
    // Should not throw with valid arguments
    const output = { env: {} as Record<string, string> }
    await expect(handler({ cwd: "/tmp" }, output)).resolves.toBeUndefined()
  })
})
