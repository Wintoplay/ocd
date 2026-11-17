// 2.3.0 session hooks: `fork` (a throwing hook still returns the saved fork) and stored message metadata on the
// messages `session.context` sees (#63235, #63590).
import { Plugin } from "@opencode/plugin"

export default Plugin.define({
  id: "fixture.fork-metadata",
  setup: async (ctx) => {
    await ctx.session.hook("prompt", (event) => {
      if (event.prompt.text.includes("canary")) event.metadata = { canary: { context: ["CANARY-TOKEN"] } }
    })
    await ctx.session.hook("context", (event) => {
      // Mirrors JavaScript plugins, which edit the payload in place.
      for (const message of event.messages as any[]) {
        const canary = message.metadata?.canary as { context: string[] } | undefined
        if (message.role !== "user" || canary === undefined) continue
        message.content.unshift({ type: "text", text: canary.context.join("\n") })
      }
    })
    await ctx.session.hook("fork", async ({ sessionID }) => {
      const seen = ((await ctx.storage.get("forks")) as string[] | undefined) ?? []
      await ctx.storage.set("forks", [...seen, sessionID])
      throw new Error("fork hook failed")
    })
    // A v1-style hooks object is not a cleanup.
    return { event: () => {} } as never
  },
})
