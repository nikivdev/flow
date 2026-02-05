/**
 * Test Extension - Demonstrates pi-mono extensibility
 *
 * Run with: pi (in ~/code/flow directory)
 * Then ask: "use the counter tool" or "run a bash command"
 */

import type { ExtensionAPI, ExtensionContext } from "@mariozechner/pi-coding-agent"
import { Type } from "@sinclair/typebox"

export default function (pi: ExtensionAPI) {
  console.log("[test-ext] Extension loaded!")

  // ============================================
  // 1. CUSTOM TOOL
  // ============================================
  let count = 0

  pi.registerTool({
    name: "counter",
    label: "Counter",
    description: "A simple counter tool. Actions: get, increment, decrement, reset",
    parameters: Type.Object({
      action: Type.Union([
        Type.Literal("get"),
        Type.Literal("increment"),
        Type.Literal("decrement"),
        Type.Literal("reset"),
      ]),
      amount: Type.Optional(Type.Number({ description: "Amount to add/subtract (default 1)" })),
    }),

    async execute(_toolCallId, params, onUpdate, _ctx, _signal) {
      const amount = params.amount ?? 1

      // Stream progress
      onUpdate?.({ content: [{ type: "text", text: `Processing ${params.action}...` }] })

      switch (params.action) {
        case "increment":
          count += amount
          break
        case "decrement":
          count -= amount
          break
        case "reset":
          count = 0
          break
      }

      return {
        content: [{ type: "text", text: `Counter is now: ${count}` }],
        details: { count, action: params.action },
      }
    },
  })

  // ============================================
  // 2. EVENT HOOKS
  // ============================================

  // Log all tool calls
  pi.on("tool_call", async (event, ctx) => {
    console.log(`[test-ext] Tool called: ${event.toolName}`)

    // Example: warn on dangerous bash commands (but don't block)
    if (event.toolName === "bash") {
      const cmd = event.input.command as string
      if (cmd.includes("rm ")) {
        ctx.ui.notify("Careful with rm commands!", "warn")
      }
    }

    return undefined // Don't block
  })

  // Log turn completions
  pi.on("turn_end", async (event, _ctx) => {
    console.log(`[test-ext] Turn ended. Tokens: ${event.usage?.inputTokens ?? 0} in, ${event.usage?.outputTokens ?? 0} out`)
  })

  // ============================================
  // 3. CUSTOM COMMAND
  // ============================================

  pi.registerCommand("count", {
    description: "Show the current counter value",
    handler: async (_args, ctx) => {
      ctx.ui.notify(`Counter: ${count}`, "info")
    },
  })

  // ============================================
  // 4. SESSION EVENTS
  // ============================================

  pi.on("session_start", async (_event, _ctx) => {
    console.log("[test-ext] Session started")
    count = 0 // Reset on new session
  })
}
