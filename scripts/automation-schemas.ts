// Regenerates assets/automation/tools.json from an opencode checkout (with node_modules):
//   bun scripts/automation-schemas.ts <opencode-checkout> assets/automation/tools.json
// The automation tools' model-facing definitions come from core's own `definition` (tool/runtime.ts), so their JSON
// Schemas are exactly what the model receives (core/src/tool/plugin/automation.ts).
const root = process.argv[2]
const out = process.argv[3]
const tools = await import(`${root}/packages/core/src/tool/plugin/automation.ts`)
const { Automation } = await import(`${root}/packages/core/src/automation.ts`)
const runtime = await import(`${root}/packages/core/src/tool/runtime.ts`)
const { Schema } = await import(`${root}/packages/core/node_modules/effect/dist/index.js`)

const ListOutput = Schema.Struct({
  now: Schema.String.annotate({ description: "The current time, in UTC." }),
  automations: Schema.Array(tools.View),
})
const RemoveOutput = Schema.Struct({ id: Automation.ID, removed: Schema.Boolean })
const definitions = [
  ["automation_create", Automation.Request, tools.View],
  ["automation_list", Schema.Struct({}), ListOutput],
  ["automation_pause", tools.IDInput, tools.View],
  ["automation_resume", tools.IDInput, tools.View],
  ["automation_delete", tools.IDInput, RemoveOutput],
].map(([name, input, output]) => {
  const definition = runtime.definition({
    name,
    description: "",
    input,
    output,
    options: { namespace: "opencode", codemode: true },
  })
  return { name, inputSchema: definition.inputSchema, outputSchema: definition.outputSchema }
})
await Bun.write(out, JSON.stringify(definitions, null, 1) + "\n")
