// Ranks a generated catalog of a few hundred tools through an upstream Code Mode package: `search(...)` queries
// (whole words, plurals, camelCase, partial words, namespaces, paging) and calls to unknown tools and namespaces
// (their "Did you mean" suggestions). Writes the catalog and every program's result for tests/search_ranking.rs.
//
//   bun run search-cases.ts <opencode>/packages/codemode crates/oc-codemode/tests/fixtures/search-2.3.0.json
const [pkg, file] = process.argv.slice(2)
const { CodeMode, Namespace, Tool } = await import(`${pkg}/src/index.ts`)
const { Effect, Schema } = await import(Bun.resolveSync("effect", `${pkg}/src`))

let seed = 20261117
const random = () => {
  seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0
  return seed / 2 ** 32
}
const pick = <T>(items: ReadonlyArray<T>): T => items[Math.floor(random() * items.length)]!

const verbs = ["list", "get", "create", "update", "delete", "search", "describe", "archive", "send", "fetch", "sync"]
const nouns = [
  "issue", "issues", "user", "users", "label", "labels", "message", "messages", "channel", "schedule", "schedules",
  "repository", "pullRequest", "CronJob", "vm", "vms", "status", "statuses", "address", "settings", "policy",
  "policies", "branch", "box", "boxes", "quiz", "bus", "analysis", "terminal", "term", "user_group", "zones",
  "timezone", "document", "documents", "order", "orders", "invoice", "webhook", "secret", "IPAddress", "v2Endpoint",
]
const fillers = ["the", "a", "of", "in", "for", "every", "open", "all", "mailbox", "project", "workspace", "team"]
const params = ["id", "userId", "query", "page_size", "labelNames", "cursor", "owner", "repo", "body", "since", "dryRun"]
const style = (words: ReadonlyArray<string>) => {
  const kind = pick(["snake", "camel", "kebab", "snake"])
  if (kind === "snake") return words.join("_")
  if (kind === "kebab") return words.join("-")
  return words.map((word, i) => (i === 0 ? word : word[0]!.toUpperCase() + word.slice(1))).join("")
}

const namespaces = [
  { path: "github", description: "GitHub issues, pull requests and repositories" },
  { path: "slack", description: "Slack channels and messages" },
  { path: "gmail", description: undefined },
  { path: "k8s", description: "Kubernetes CronJob and pod management" },
  { path: "cloud", description: "Cloud virtual machines" },
  { path: "cloud.compute", description: "Compute instances (VMs)" },
  { path: "cloud.network", description: "Networks, zones and IP addresses" },
  { path: "jira", description: "Issue tracking" },
  { path: "MyService", description: "An internal service" },
  { path: "weird-name", description: "A server whose name is not an identifier" },
]

type Entry = { path: string; description: string; params: Array<string> }
const catalog: Array<Entry> = []
const seen = new Set<string>()
while (catalog.length < 400) {
  const ns = pick(namespaces).path
  const words = [pick(verbs), pick(nouns), ...(random() < 0.3 ? [pick(nouns)] : [])]
  const path = `${ns}.${style(words)}`
  if (seen.has(path) || namespaces.some((n) => n.path === path)) continue
  seen.add(path)
  const description = [pick(verbs), pick(fillers), pick(nouns), pick(fillers), pick(nouns)]
    .slice(0, 2 + Math.floor(random() * 4))
    .join(" ")
  const count = Math.floor(random() * 4)
  const names = [...new Set(Array.from({ length: count }, () => pick(params)))]
  catalog.push({ path, description: random() < 0.05 ? "" : description, params: names })
}

// The catalog as upstream tools, nested through Namespace.make for namespaces with metadata.
const tool = (entry: Entry) =>
  Tool.make({
    description: entry.description,
    input: Schema.Struct(Object.fromEntries(entry.params.map((name) => [name, Schema.String]))),
    output: Schema.String,
    execute: () => Effect.succeed("ok"),
  })
const tools: Record<string, unknown> = {}
for (const ns of namespaces) {
  const children: Record<string, unknown> = {}
  for (const entry of catalog)
    if (entry.path.startsWith(`${ns.path}.`) && !entry.path.slice(ns.path.length + 1).includes("."))
      children[entry.path.slice(ns.path.length + 1)] = tool(entry)
  tools[ns.path] = Namespace.make({ ...(ns.description === undefined ? {} : { description: ns.description }), tools: children })
}
const runtime = CodeMode.make({ tools, limits: { timeoutMs: 5000 } })

const programs: Array<string> = []
const search = (request: Record<string, unknown>) =>
  programs.push(
    `const r = search(${JSON.stringify(request)}); return { paths: r.items.map((item) => item.path), remaining: r.remaining, next: r.next }`,
  )
const queries = [
  "", "   ", "*", "list", "issues", "issue", "list issues", "users", "user groups", "listUsers", "list_user_groups",
  "CronJob", "cronjob", "cron job", "vms", "vm", "zones", "timezones", "schedu", "sched", "repo", "repos",
  "pull request", "pullRequest", "statuses", "status", "policies", "boxes", "quizzes", "buses", "analyses",
  "terminal", "term", "termi", "ip address", "IPAddress", "v2", "endpoint", "github issues", "slack message",
  "send the email", "archive thread", "delete secret", "webhooks", "invoice orders", "documents", "docum",
  "tools.github", "kubernetes", "compute instances", "networks", "internal", "identifier", "label names", "dryRun",
  "page size", "userId", "a", "the of in", "zzz", "Ünïcode", "K", "get-users", "weird-name",
]
for (const query of queries) search({ query })
for (let i = 0; i < 120; i++) {
  const words = Array.from({ length: 1 + Math.floor(random() * 3) }, () =>
    random() < 0.15 ? pick(nouns).slice(0, 4 + Math.floor(random() * 3)) : pick([...verbs, ...nouns, ...fillers]),
  )
  search({ query: words.join(" ") })
}
for (const ns of ["github", "slack", "cloud", "cloud.compute", "tools.k8s", "weird-name", "jira"])
  for (const query of ["list", "users", "", "vms issues"]) search({ query, namespace: ns })
for (const [offset, limit] of [[0, 3], [3, 3], [10, 25], [0, 1000]]) search({ query: "list users", offset, limit })
// Exact paths short-circuit ranking.
for (const entry of catalog.slice(0, 5)) search({ query: entry.path })
search({ query: `tools.${catalog[7]!.path}` })

// Unknown tools and namespaces.
const call = (expression: string) => programs.push(`return await ${expression}`)
for (const name of ["list_issue", "listIssues", "users", "nope", "schedules", "get_vm", "cronjob"])
  for (const ns of ["github", "slack", "k8s", "cloud", "cloud.compute"]) call(`tools.${ns}.${name}({})`)
for (const name of ["issues", "list", "sendMessage", "terminal", "zzz"]) call(`tools.${name}({})`)
for (const ns of ["github", "slack", "gmail", "cloud", "cloud.compute", "cloud.network", "jira", "MyService"])
  call(`tools.${ns}({})`)
call(`tools["weird-name"]({})`)
call(`tools.cloud["compute"]({})`)

const cases = []
for (const code of programs) {
  // Normalize the way the Rust side compares: a round trip through JSON text.
  const result = JSON.parse(JSON.stringify(await Effect.runPromise(runtime.execute(code))))
  cases.push({ code, result })
}
await Bun.write(file, JSON.stringify({ namespaces, catalog, cases }, null, 1) + "\n")
console.log(`${catalog.length} tools, ${cases.length} cases`)
