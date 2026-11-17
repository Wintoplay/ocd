// Re-runs the programs of tests/fixtures/vm-2.3.0.json through an upstream Code Mode package and rewrites their
// results (each `gap` note is kept). Run from anywhere with the package's dependencies installed:
//
//   bun run vm-cases.ts <opencode>/packages/codemode crates/oc-codemode/tests/fixtures/vm-2.3.0.json
const [pkg, file] = process.argv.slice(2)
const { CodeMode } = await import(`${pkg}/src/index.ts`)
const { Effect } = await import(Bun.resolveSync("effect", `${pkg}/src`))
const runtime = CodeMode.make({ limits: { timeoutMs: 5000 } })
const cases: Array<{ code: string; result: unknown; gap?: string }> = await Bun.file(file).json()
let changed = 0
for (const item of cases) {
  // Normalize the way the Rust side compares: a round trip through JSON text.
  const result = JSON.parse(JSON.stringify(await Effect.runPromise(runtime.execute(item.code))))
  if (JSON.stringify(result) !== JSON.stringify(item.result)) changed++
  item.result = result
}
await Bun.write(file, JSON.stringify(cases, null, 1) + "\n")
console.log(`${cases.length} cases, ${changed} results changed`)
