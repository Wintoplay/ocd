"""opencode 2.3.0 Code Mode: compact JSON results, `search` ranked by name-weighted BM25, the "Did you mean" of an
unknown tool and of a namespace call, and streaming `fetch` responses (a real `Headers`, a `body` stream read in
chunks, one-shot whole-body readers, a redirect's body discarded, a body still open when the program ends cancelled)."""
import select
import time

from parity.loop import run_prompt
from parity.registry import scenario
from parity.tools22 import _echo_mcp, _mcp_connected


def execute(code, id):
    return {"name": "execute", "args": {"code": code}, "id": id}


def _chunked(chunks, cancelled=None, name=None):
    """A route sending `chunks` 80 ms apart (well clear of the 100 ms progress throttle, so the progress rows do not
    race the reads) (HTTP/1.0, read until close). With `cancelled`, the body stays open after
    the chunks until the client goes away, and `name` is appended to `cancelled` then."""
    def route(handler):
        handler.send_response(200)
        handler.send_header("content-type", "text/plain;charset=utf-8")
        handler.end_headers()
        for chunk in chunks:
            handler.wfile.write(chunk)
            handler.wfile.flush()
            time.sleep(0.08)
        if cancelled is None:
            return
        _wait_closed(handler, cancelled, name)
    return route


def _wait_closed(handler, cancelled, name):
    """Waits (up to 20 s) for the client to close or reset the connection, then records `name`."""
    deadline = time.time() + 20
    while time.time() < deadline:
        ready, _, _ = select.select([handler.connection], [], [], 0.05)
        if not ready:
            continue
        try:
            if handler.connection.recv(1):
                continue
        except OSError:
            pass
        cancelled.append(name)
        return


@scenario("codemode230")
def codemode230_search(ctx):
    """search over the built-in and MCP tools (whole words, partial words, plurals, a namespace filter), an unknown
    tool's suggestion, calls to namespaces (up to three tools named, the rest counted), and a compact JSON result."""
    searches = "\n".join([
        "const q = (query, namespace) => search({ query, namespace, limit: 5 }).items.map((item) => item.path)",
        "return { read: q(\"read file\"), shell: q(\"run shell commands\"), partial: q(\"sched\"), plural: q(\"todos\"),",
        "  web: q(\"fetch url\"), typed: q(\"typed\", \"fake\"), none: q(\"zzzz\"), nested: { a: [1, { b: null }] } }",
    ])
    run_prompt(ctx, [
        {"tools": [execute(searches, "call_search")]},
        {"tools": [execute("return await tools.fake.typd({})", "call_unknown")]},
        {"tools": [execute("return await tools.fake({})", "call_namespace")]},
        {"tools": [execute("return await tools.opencode({})", "call_builtin_namespace")]},
        {"text": "Done."},
    ], text="search", config_extra=_echo_mcp(), before=_mcp_connected)


@scenario("codemode230")
def codemode230_fetch_stream(ctx):
    """fetch streams the body: chunks read with a reader and with `for await` (decoded with a streaming TextDecoder),
    a second whole-body read fails, the response's keys and `Headers`, a redirect whose open body is discarded, and a
    body still open when the program returns is cancelled."""
    base = f"http://127.0.0.1:{ctx.mock.port}"
    cancelled = []
    ctx.mock.routes["/chunks"] = _chunked([b"ab", b"cd", b"ef"])
    ctx.mock.routes["/split"] = _chunked([b"\xc3", b"\xa9\xf0\x9f", b"\x98\x80"])
    ctx.mock.routes["/open"] = _chunked([b"*"], cancelled, "open")
    ctx.mock.routes["/json"] = (200, {"a": 1, "b": [1, 2]})

    def redirect(handler):
        handler.send_response(302)
        handler.send_header("location", "/json")
        handler.end_headers()
        handler.wfile.write(b"moved")
        handler.wfile.flush()
        _wait_closed(handler, cancelled, "redirect")
    ctx.mock.routes["/redirect"] = redirect
    reads = "\n".join([
        f'const res = await fetch("{base}/chunks")',
        "const keys = Object.keys(res)",
        "const reader = res.body.getReader()",
        "let text = \"\"",
        "for (;;) { const { value, done } = await reader.read(); if (done) break; text += new TextDecoder().decode(value) }",
        "const again = await res.text().catch((e) => [e.name, e.message])",
        f'const split = await fetch("{base}/split")',
        "const d = new TextDecoder(); let chars = \"\"",
        "for await (const chunk of split.body) chars += d.decode(chunk, { stream: true })",
        f'const json = await fetch("{base}/json")',
        "const before = json.bodyUsed",
        "const value = await json.json()",
        "return { keys, text, again, bodyUsed: res.bodyUsed, locked: res.body.locked, chars: chars + d.decode(),",
        "  before, value, after: json.bodyUsed, headers: json.headers instanceof Headers,",
        "  type: json.headers.get(\"Content-Type\"), body: json.body === null }",
    ])
    redirected = "\n".join([
        f'const res = await fetch("{base}/redirect")',
        "return [res.redirected, res.status, res.url.endsWith(\"/json\"), await res.json()]",
    ])
    left_open = "\n".join([
        f'const res = await fetch("{base}/open")',
        "const reader = res.body.getReader()",
        "return (await reader.read()).value[0]",
    ])
    run_prompt(ctx, [
        {"tools": [execute(reads, "call_reads")]},
        {"tools": [execute(redirected, "call_redirect")]},
        {"tools": [execute(left_open, "call_open")]},
        {"text": "Done."},
    ], text="fetch", config_extra={"permission": {"webfetch": "allow"}})
    deadline = time.time() + 10
    while time.time() < deadline and len(cancelled) < 2:
        time.sleep(0.05)
    ctx.rec("cancelled", sorted(cancelled))
    ctx.rec("fetched", [r["path"] for r in ctx.mock.requests if r["kind"] == "GET"])
