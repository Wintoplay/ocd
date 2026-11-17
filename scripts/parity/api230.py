"""2.3.0 API and config changes: the new flag keys (`shell_timeout_background`, `mcp_events`), `codemode_vm` as a
deprecated key and the retired `codemodeVm` alias, provider `settings.tls` (echoed, never a model setting or a
provider option), model `capabilities.freeform` in config, and the new public event types."""
import json
import urllib.error
import urllib.request

from parity.registry import scenario


@scenario("api230")
def api230_config_keys(ctx):
    """config accepts and echoes 2.3.0's keys in its order: the flags shell_timeout_background and mcp_events,
    codemode_vm (deprecated, the codemodeVm alias ignored), provider settings.tls (malformed ones invalidate the
    provider), model capabilities.freeform; tls stays out of model settings; the event filter takes the new types."""
    ctx.set_script([])
    ctx.make_project(config_extra={
        "experimental": {"codemodeVm": False, "mcp_events": True, "shell_timeout_background": False,
                         "codemode_vm": True, "console_watches": True, "all_flags": False},
        "providers": {
            "mock": {
                "name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": f"http://127.0.0.1:{ctx.mock.port}/v1", "apiKey": "test",
                             "tls": {"key": "KEY", "cert": "{file:./missing.pem}", "extra": 1}, "encoding": "gzip"},
                "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096},
                                  "capabilities": {"freeform": ["lark", "text"], "tools": True, "temperature": False},
                                  "settings": {"tls": {"cert": "a", "key": "b"}, "reasoningEffort": "low"}}},
            },
            "bad_tls": {"settings": {"tls": {"cert": 1, "key": "k"}}},
            "bad_tls_key": {"settings": {"tls": {"cert": "c"}}},
            "bad_freeform": {"models": {"m": {"capabilities": {"freeform": ["json"]}}}},
        },
    })
    ctx.start()
    ctx.booted()
    ctx.call("config", "GET", "/api/config")
    ctx.settle("/api/provider")
    _, providers = ctx.call("provider.list", "GET", "/api/provider", record=False)
    ctx.rec("provider.list mock", [p for p in providers.get("data", []) if p.get("id") == "mock"])
    _, models = ctx.call("model.list", "GET", "/api/model", record=False)
    ctx.rec("model.list mock", [m for m in models.get("data", []) if m.get("providerID") == "mock"])
    for types in ["session.step.first-token", "experimental.mcp.event&types=session.step.first-token",
                  "experimental.mcp.subscription.changed"]:
        ctx.rec(f"event.subscribe ?types={types}", subscribe(ctx, types))


def subscribe(ctx, types):
    """`GET /api/event?types=…`: the status and content type of a stream (closed at once), or the error body."""
    req = urllib.request.Request(f"{ctx.server.url}/api/event?types={types}")
    req.add_header("authorization", ctx.server.auth)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            first = resp.readline().decode("utf-8", "replace")
            return {"status": resp.status, "content-type": resp.headers.get("content-type"),
                    "first": json.loads(first[5:])["type"] if first.startswith("data:") else first}
    except urllib.error.HTTPError as e:
        return {"status": e.code, "body": json.loads(e.read() or b"null")}
