"""opencode 2.3.1 Code Mode: Node's `Buffer`, tool arguments crossing as host copies then JSON (functions left out,
promises awaited, a program's `toJSON` not run, iterators and byte arrays refused), `search("text")`, the reworded
messages of the language/Code Mode split, no `null` line before a logging program's logs, plugin renames of calls
inside a program, and a nested call's permission request shown on the `execute` call's row."""
from parity.codemode230 import execute
from parity.loop import run_prompt
from parity.plugins import boot
from parity.registry import scenario
from parity.tools22 import _echo_mcp, _mcp_connected


@scenario("codemode231")
def codemode231_buffer(ctx):
    """Buffer: text in each encoding, number reads and writes, views, Node's error codes, the `buffer` module, a
    Buffer returned (its toJSON), and `console.log` of a Buffer in a program that returns nothing (no null line)."""
    texts = "\n".join([
        'const b = Buffer.from("héllo wörld ☃")',
        'return { length: b.length, hex: b.toString("hex"), base64: b.toString("base64"), url: b.toString("base64url"),',
        '  latin1: Buffer.from("é", "latin1").toString("hex"), ucs2: Buffer.from("ab", "ucs2").toString("hex"),',
        '  decoded: Buffer.from("aGk=", "base64").toString(), slice: b.subarray(1, 3).toString(),',
        '  json: JSON.stringify(Buffer.from("hi")), isBuffer: [Buffer.isBuffer(b), Buffer.isBuffer(new Uint8Array(1))],',
        '  concat: Buffer.concat([Buffer.from("a"), Buffer.from("b")]).toString(), byteLength: Buffer.byteLength("😀") }',
    ])
    numbers = "\n".join([
        "const b = Buffer.alloc(8)",
        "b.writeUInt16BE(0x1234, 0); b.writeInt8(-1, 2); b.writeFloatLE(1.5, 4)",
        "const errors = []",
        "for (const run of [() => b.readUInt32LE(6), () => b.writeUInt8(256), () => Buffer.from(\"x\", \"nope\"),",
        "  () => b.readUInt8(\"1\"), () => Buffer.alloc(-1), () => b.swap32.call(Buffer.alloc(3))])",
        "  try { run() } catch (e) { errors.push([e.name, e.code, e.message]) }",
        "return { hex: b.toString(\"hex\"), reads: [b.readUInt16BE(0), b.readInt8(2), b.readFloatLE(4)], errors }",
    ])
    modules = 'import { Buffer as B } from "node:buffer"\nreturn [B === Buffer, Object.keys(require("buffer")), Buffer.from("x")]'
    logs = 'console.log(Buffer.from("hi"), [Buffer.alloc(2)])\nconsole.log(String(Buffer.from("text")))'
    run_prompt(ctx, [
        {"tools": [execute(texts, "call_texts")]},
        {"tools": [execute(numbers, "call_numbers")]},
        {"tools": [execute(modules, "call_modules")]},
        {"tools": [execute(logs, "call_logs")]},
        {"text": "Done."},
    ], text="buffer")


@scenario("codemode231")
def codemode231_crossing(ctx):
    """Arguments of MCP tool calls cross as host copies, then JSON: a `toJSON` is not run, functions are left out,
    promises awaited, Dates, Sets, URLs and errors converted; an iterator, a symbol, `tools` itself and a Uint8Array
    are refused; `search("text")` and its input error's suggestion; the reworded `tools`, import and await messages."""
    crossing = "\n".join([
        "return await tools.fake.open({ count: 1, price: { cents: 1999, toJSON() { return \"$19.99\" } },",
        "  format(v) { return v }, later: Promise.resolve([1, () => 2]), when: new Date(0), tags: new Set([\"a\", \"b\"]),",
        "  url: new URL(\"https://a.test/p?q=1\"), error: Object.assign(new TypeError(\"bad\"), { code: \"E1\" }) })",
    ])
    refused = "\n".join([
        "const out = []",
        "for (const value of [[1].keys(), Symbol.iterator, tools, new Uint8Array([1]), Buffer.from(\"x\")])",
        "  try { await tools.fake.open({ value }) } catch (e) { out.push([e.name, e.message]) }",
        "try { await tools.fake.open({ p: Promise.reject(new Error(\"no price\")) }) } catch (e) { out.push(e.message) }",
        "return out",
    ])
    searches = "\n".join([
        "const paths = (result) => result.items.map((item) => item.path)",
        "return [paths(search(\"typed\")), paths(await search(\"open\")), paths(await tools.search(\"echo\"))]",
    ])
    messages = "\n".join([
        "const out = []",
        "for (const run of [() => tools(), () => Object.values(tools), () => tools[Symbol.iterator], () => require(\"yaml\"),",
        "  () => [1].map(tools.fake.open), () => Promise.resolve(1).value, () => structuredClone(tools)])",
        "  try { run() } catch (e) { out.push(e.message) }",
        "return out",
    ])
    run_prompt(ctx, [
        {"tools": [execute(crossing, "call_crossing")]},
        {"tools": [execute(refused, "call_refused")]},
        {"tools": [execute(searches, "call_search")]},
        {"tools": [execute("return search(42)", "call_search_error")]},
        {"tools": [execute(messages, "call_messages")]},
        {"tools": [execute("await new Promise(() => {})", "call_stalled")]},
        {"text": "Done."},
    ], text="crossing", config_extra=_echo_mcp(), before=_mcp_connected)


@scenario("codemode231")
def codemode231_plugin_rename(ctx):
    """A plugin's tool.execute.before renames calls inside a program: to another Code Mode tool (its structured
    output returned), and to a direct or unknown tool (not available for this request)."""
    boot(ctx, ["./plugins/codemode-rename-plugin"], ["codemode-rename-plugin"], script=[
        {"tools": [execute('return await tools.rename.relay({ text: "to:constant" })', "call_constant")]},
        {"tools": [execute("return await tools.rename.plain({})", "call_plain")]},
        {"tools": [execute('return await tools.rename.relay({ text: "kept" })', "call_kept")]},
        {"tools": [execute("const out = []\nfor (const name of [\"direct\", \"missing\"])\n"
                           "  try { await tools.rename.relay({ text: `to:${name}` }) } catch (e) { out.push(e.message) }\n"
                           "return out", "call_unavailable")]},
        {"text": "Renamed."},
    ])
    sid = ctx.create_session()
    ctx.prompt(sid, "rename")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("codemode231")
def codemode231_nested_permission(ctx):
    """A call inside a program asks a permission (worktree_remove asks `edit`): the request names the `execute`
    call as its source, so it shows on that call's row. Allowed once, the removal then fails (the project directory
    is no worktree) and the program sees that."""
    remove = "\n".join([
        "try { return await tools.opencode.worktree_remove({ directory: \".\" }) }",
        "catch (e) { return [e.name, e.message] }",
    ])
    ctx.set_script([{"tools": [execute(remove, "call_remove")]}, {"text": "Answered."}])
    ctx.make_project(config_extra={"permission": {"edit": "ask"}})
    ctx.start()
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "remove the worktree", wait=False)
    ev = ctx.wait_for("permission.asked", start=mark, timeout=30)
    if ev:
        pid = ev["data"]["id"]
        ctx.call("permission.get", "GET", f"/api/session/{sid}/permission/{pid}")
        ctx.call("permission.reply", "POST", f"/api/session/{sid}/permission/{pid}/reply", {"decision": "once"})
    ctx.wait_idle(sid, start=mark, timeout=30)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()
