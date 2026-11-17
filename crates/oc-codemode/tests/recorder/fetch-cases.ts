// Runs Code Mode programs that read streaming `fetch` responses through upstream core's `execute` tool (its Code Mode
// VM and `codemode/fetch.ts`) against a local server, and writes each program's `execute` output for
// tests/fetch_stream.rs, which serves the same routes. The server's origin is written as `http://BASE`.
//
//   bun run fetch-cases.ts <opencode> crates/oc-codemode/tests/fixtures/fetch-2.3.0.json
//
// Every route also has to be served the same way by tests/fetch_stream.rs: keep them in sync.
const [root, file] = process.argv.slice(2)
const { CodeModeTool } = await import(`${root}/packages/core/src/codemode/tool.ts`)
const { execute } = await import(`${root}/packages/core/src/tool/runtime.ts`)
const { permissionService } = await import(`${root}/packages/core/test/lib/permission.ts`)
const { Effect } = await import(Bun.resolveSync("effect", `${root}/packages/core/src`))

const encode = (text: string) => new TextEncoder().encode(text)
const closed: Array<string> = []
// A body that sends `first` and stays open until it is cancelled.
const open = (name: string, first: Uint8Array) =>
  new ReadableStream({
    start(controller) {
      controller.enqueue(first)
    },
    cancel() {
      closed.push(name)
    },
  })
// A body sent in separate chunks.
const chunked = (chunks: Array<Uint8Array>) =>
  new ReadableStream({
    async start(controller) {
      for (const chunk of chunks) {
        controller.enqueue(chunk)
        await Bun.sleep(20)
      }
      controller.close()
    },
  })

const server = Bun.serve({
  port: 0,
  hostname: "127.0.0.1",
  fetch(req) {
    const path = new URL(req.url).pathname
    const text = (body: string | Uint8Array | ReadableStream | null, init: ResponseInit = {}) =>
      new Response(body, { ...init, headers: { "content-type": "text/plain;charset=utf-8", ...init.headers } })
    switch (path) {
      case "/json":
        return new Response('{"a":1,"b":[1,2]}', { headers: { "content-type": "application/json;charset=utf-8" } })
      case "/text":
        return text("hello world")
      case "/chunks":
        return text(chunked([encode("ab"), encode("cd"), encode("ef")]))
      case "/split":
        // "é😀" with both characters split across chunks.
        return text(chunked([Uint8Array.of(0xc3), Uint8Array.of(0xa9, 0xf0, 0x9f), Uint8Array.of(0x98, 0x80)]))
      case "/open":
        return text(open("open", Uint8Array.of(42)))
      case "/empty":
        return text("")
      case "/nocontent":
        return new Response(null, { status: 204 })
      case "/missing":
        return text("no such thing", { status: 404 })
      case "/redirect":
        return text(open("redirect", encode("moved")), { status: 302, headers: { location: "/json" } })
      case "/headers":
        return new Response("h", {
          headers: [
            ["content-type", "text/plain;charset=utf-8"],
            ["x-b", "2"],
            ["set-cookie", "a=1"],
            ["x-a", "1"],
            ["set-cookie", "b=2"],
            ["x-a", "3"],
          ],
        })
      case "/bom":
        return text(Uint8Array.of(0xef, 0xbb, 0xbf, 0x68, 0x69, 0xff))
      case "/bad-json":
        return text("not json")
      default:
        return text("unknown route", { status: 500 })
    }
  },
})
const base = `http://127.0.0.1:${server.port}`

const allowAll = permissionService({ assert: () => Effect.void, assertNotDenied: () => Effect.void })
const codeMode = CodeModeTool.create(
  { tools: new Map() },
  (_: unknown, tool: any, input: unknown, context: any) => execute(tool, input, context),
  allowAll,
)
const context = {
  sessionID: "ses_fetch",
  agent: "build",
  messageID: "msg_fetch",
  id: "call_fetch",
  progress: () => Effect.void,
}

const fetchAt = (path: string, init?: string) => `const res = await fetch(BASE + ${JSON.stringify(path)}${init ? `, ${init}` : ""});`
const programs: Array<string> = [
  // The response object.
  `${fetchAt("/json")} return Object.keys(res)`,
  `${fetchAt("/json")} return res`,
  `${fetchAt("/json")} return JSON.stringify(res)`,
  `${fetchAt("/json")} return [res.status, res.ok, res.statusText, res.redirected, res.bodyUsed, res.body === null, typeof res.body, res.body.locked, res.headers instanceof Headers, res.url === BASE + "/json"]`,
  `${fetchAt("/json")} return [typeof res.text, res.text.name, res.text.length, res.json.length, res.body.getReader.name, res.body.getReader.length, res.body.cancel.length, typeof res.body[Symbol.asyncIterator], Object.keys(res.body)]`,
  `${fetchAt("/json")} return res.body`,
  `${fetchAt("/json")} return res.headers`,
  `${fetchAt("/missing")} return [res.status, res.ok, res.statusText, await res.text()]`,
  // Whole-body readers consume the body once.
  `${fetchAt("/json")} const a = await res.json(); const b = await res.text().catch(e => [e.name, e.message]); return [a, b, res.bodyUsed, res.body.locked]`,
  `${fetchAt("/json")} const t = await res.text(); return [t, await res.json().catch(e => [e.name, e.message]), await res.bytes().catch(e => e.name)]`,
  `${fetchAt("/json")} const b = await res.bytes(); return [b instanceof Uint8Array, [...b].length, b[0]]`,
  `${fetchAt("/json")} const b = await res.arrayBuffer(); return [b instanceof Uint8Array, b.length]`,
  `${fetchAt("/text")} return await res.json().catch(e => [e.name, e.message])`,
  `${fetchAt("/bad-json")} return await res.json().catch(e => [e.name, e.message])`,
  `${fetchAt("/empty")} return [res.body === null, await res.text(), res.bodyUsed, await res.text().catch(e => e.message)]`,
  `${fetchAt("/empty")} return await res.json().catch(e => [e.name, e.message])`,
  `${fetchAt("/bom")} return [await res.text()]`,
  `${fetchAt("/chunks")} return await res.text()`,
  // Bodies that are null.
  `${fetchAt("/nocontent")} return [res.status, res.body, res.bodyUsed, await res.text(), await res.text(), res.bodyUsed, [...await res.bytes()], await res.json().catch(e => [e.name, e.message])]`,
  `${fetchAt("/json", '{ method: "HEAD" }')} return [res.body, res.bodyUsed, await res.text(), await res.text()]`,
  // Readers.
  `${fetchAt("/chunks")} const r = res.body.getReader(); const out = []; for (;;) { const { value, done } = await r.read(); if (done) break; out.push(...value) } return [new TextDecoder().decode(new Uint8Array(out)), res.bodyUsed, res.body.locked]`,
  `${fetchAt("/json")} const r = res.body.getReader(); return [Object.keys(r), r.read.name, r.read.length, r.cancel.name, r.cancel.length, r.releaseLock.name, r.releaseLock.length]`,
  `${fetchAt("/json")} const r = res.body.getReader(); const a = await r.read(); const b = await r.read(); const c = await r.read(); return [Object.keys(a), a.done, a.value instanceof Uint8Array, Object.keys(b), b, c.done, res.body.locked]`,
  `${fetchAt("/json")} const r = res.body.getReader(); return [res.bodyUsed, res.body.locked, await res.text().catch(e => [e.name, e.message])]`,
  `${fetchAt("/json")} res.body.getReader(); try { res.body.getReader() } catch (e) { return [e.name, e.message] }`,
  `${fetchAt("/json")} try { res.body.getReader({ mode: "byob" }) } catch (e) { return [e.name, e.message, res.body.locked] }`,
  `${fetchAt("/json")} try { res.body.getReader({ mode: null }) } catch (e) { return [e.name, e.message] }`,
  `${fetchAt("/json")} res.body.getReader({}); res.body.getReader === undefined; return res.body.locked`,
  `${fetchAt("/json")} const r = res.body.getReader(); r.releaseLock(); r.releaseLock(); return [res.body.locked, res.bodyUsed, await r.read().catch(e => [e.name, e.message]), await r.cancel().catch(e => e.message), await res.text().catch(e => e.message)]`,
  `${fetchAt("/json")} const r = res.body.getReader(); await r.read(); r.releaseLock(); const again = res.body.getReader(); return [await again.read(), res.body.locked]`,
  `${fetchAt("/json")} res.body.getReader(); return await res.body.cancel().catch(e => [e.name, e.message])`,
  `${fetchAt("/json")} const c = await res.body.cancel("stop"); return [c, res.bodyUsed, res.body.locked, await res.body.cancel(), await res.body.getReader().read(), await res.text().catch(e => e.message)]`,
  `${fetchAt("/json")} const r = res.body.getReader(); const c = await r.cancel(); return [c, await r.read(), res.body.locked, res.bodyUsed]`,
  `${fetchAt("/open")} const r = res.body.getReader(); return (await r.read()).value[0]`,
  `${fetchAt("/open")} const r = res.body.getReader(); const chunk = await r.read(); await r.cancel(); return [chunk.value[0], await r.read()]`,
  // Async iteration.
  `${fetchAt("/chunks")} const d = new TextDecoder(); let text = ""; for await (const chunk of res.body) text += d.decode(chunk, { stream: true }); return [text + d.decode(), res.body.locked, res.bodyUsed]`,
  `${fetchAt("/split")} const d = new TextDecoder(); let text = ""; for await (const chunk of res.body) text += d.decode(chunk, { stream: true }); return [text + d.decode(), [...text].length]`,
  `${fetchAt("/open")} let first; for await (const chunk of res.body) { first = chunk[0]; break } return [first, res.body.locked, res.bodyUsed]`,
  `${fetchAt("/json")} const it = res.body[Symbol.asyncIterator](); const a = await it.next(); const b = await it.next(); return [Object.keys(it), it.next.name, it.return.name, Object.keys(a), a.done, b, res.body.locked]`,
  `${fetchAt("/json")} const it = res.body[Symbol.asyncIterator](); const r = await it.return(); return [r, res.body.locked, res.bodyUsed, await it.next(), await it.return()]`,
  `${fetchAt("/json")} res.body.getReader(); try { for await (const c of res.body) {} } catch (e) { return [e.name, e.message] }`,
  `${fetchAt("/json")} const it = res.body[Symbol.asyncIterator](); return [res.body.locked, res.bodyUsed, await res.text().catch(e => e.message)]`,
  // Headers.
  `${fetchAt("/headers")} return [[...res.headers].filter(([k]) => k !== "date" && k !== "content-length"), res.headers.get("X-A"), res.headers.getSetCookie(), res.headers.has("x-b"), typeof res.headers.entries().next]`,
  `${fetchAt("/headers")} const h = new Headers(res.headers); h.set("x-c", "4"); return [h.get("x-c"), res.headers.get("x-c"), res.headers.get("content-type")]`,
  // Redirects.
  `${fetchAt("/redirect")} return [res.redirected, res.url === BASE + "/json", res.status, await res.json()]`,
  // A response can be returned from a function and read later.
  `const get = async (p) => (await fetch(BASE + p)).text(); return await Promise.all([get("/text"), get("/json"), get("/chunks")])`,
]

const cases = []
for (const code of programs) {
  const result = await Effect.runPromise(
    codeMode.execute({ code: `const BASE = ${JSON.stringify(base)}; ${code}` }, context as never),
  )
  // The server's origin and the Date header vary.
  const output = JSON.parse(
    JSON.stringify(result.output)
      .replaceAll(base, "http://BASE")
      .replace(/[A-Z][a-z]{2}, \d\d [A-Z][a-z]{2} \d{4} \d\d:\d\d:\d\d GMT/g, "DATE"),
  )
  cases.push({ code, output })
}
// Cancelled bodies the server saw, once every program has run.
await Bun.sleep(200)
await Bun.write(file, JSON.stringify({ cases, closed: closed.sort() }, null, 1) + "\n")
server.stop(true)
console.log(`${cases.length} cases, closed: ${closed.join(", ")}`)
