// Code Mode tools and a tool.execute.before hook that renames calls to them (opencode 2.3.1, #63633/#64333): a
// `relay` call goes to the tool its `text` names, a `plain` call to `constant`. `direct` is not a Code Mode tool.
import { Plugin } from "@opencode/plugin"
import { Schema } from "effect"

const Text = Schema.Struct({ text: Schema.String })

export default Plugin.define({
  id: "fixture.codemode-rename",
  setup: async (ctx) => {
    await ctx.tool.transform((tools) => {
      tools.add({
        name: "relay",
        options: { codemode: true, namespace: "rename" },
        description: "Relay text",
        input: Text,
        output: Text,
        execute: async ({ text }) => ({ output: { text }, content: `relay: ${text}` }),
      })
      tools.add({
        name: "constant",
        options: { codemode: true, namespace: "rename" },
        description: "Answer with a constant",
        input: Schema.Struct({}),
        output: Text,
        execute: async () => ({ output: { text: "renamed" }, content: "constant" }),
      })
      tools.add({
        name: "plain",
        options: { codemode: true, namespace: "rename" },
        description: "Answer with text",
        input: Schema.Struct({}),
        execute: async () => ({ content: "plain" }),
      })
      tools.add({
        name: "direct",
        options: { codemode: false },
        description: "A direct tool",
        input: Schema.Struct({}),
        execute: async () => ({ content: "direct" }),
      })
    })
    await ctx.tool.hook("execute.before", (event) => {
      const input = event.input as { text?: unknown }
      if (event.tool === "relay" && typeof input?.text === "string" && input.text.startsWith("to:")) {
        event.tool = input.text.slice(3)
        event.input = {}
      }
      if (event.tool === "plain") event.tool = "constant"
    })
  },
})
