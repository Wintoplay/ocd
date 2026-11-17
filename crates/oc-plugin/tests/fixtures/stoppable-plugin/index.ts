// Promise tools that stop cooperatively through their signal (opencode 2.3.0, promise-tool-cancel.test.ts).
import { Plugin } from "@opencode/plugin"

const empty = { type: "object", properties: {}, additionalProperties: false } as const

export default Plugin.define({
  id: "fixture.stoppable",
  setup: async (ctx) => {
    await ctx.tool.transform((editor) => {
      editor.add({
        name: "listens",
        description: "Rejects once stopped",
        input: empty,
        options: { codemode: false },
        execute: (_input, context) =>
          new Promise<never>((_resolve, reject) => {
            const signal = context.signal
            signal.addEventListener("abort", () => reject(new Error("stopped with partial work")), { once: true })
          }),
      })
      editor.add({
        name: "resolves",
        description: "Resolves with what it has once stopped",
        input: empty,
        options: { codemode: false },
        execute: (_input, context) =>
          new Promise<{ content: string }>((resolve) => {
            const signal = context.signal
            signal.addEventListener("abort", () => resolve({ content: "partial result" }), { once: true })
          }),
      })
      editor.add({
        name: "deaf",
        description: "Never reads its signal",
        input: empty,
        options: { codemode: false },
        execute: () => new Promise<{ content: string }>((resolve) => setTimeout(() => resolve({ content: "done" }), 300)),
      })
    })
  },
})
