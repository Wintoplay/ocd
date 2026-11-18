"""2.3.1 API and config changes: `compaction.background`, the reworked flag registry (`automations` with `schedules`
as its old name, `video_generate`, the opt-in-only `full_codemode`; `background_compaction` and
`shell_timeout_background` retired), the schedule routes replaced by automation routes (answering as disabled), the
video routes behind `video_generate`, the blob route, and the `opencode.tool.automation` built-in plugin."""
import os

from parity.registry import scenario

MISSING_SESSION = "ses_ffffffffffffZZZZZZZZZZZZZZ"
MISSING_AUTOMATION = "aut_ffffffffffffZZZZZZZZZZZZZZ"
BLOB = "video_0123456789abABCDEFGHIJKLMN_0"
TEXT_BLOB = "video_0123456789abABCDEFGHIJKLMN_1"


@scenario("api231")
def api231_config_keys(ctx):
    """config accepts and echoes 2.3.1's keys in its order: compaction.background after auto, the flags automations,
    video_generate and full_codemode, schedules after the registry; the retired background_compaction and
    shell_timeout_background are dropped; invalid values are reported."""
    ctx.set_script([])
    ctx.make_project(config_extra={
        "compaction": {"buffer": 4000, "background": False, "auto": True},
        "experimental": {"schedules": True, "full_codemode": True, "shell_timeout_background": True,
                         "video_generate": False, "background_compaction": True, "automations": False,
                         "media_blob": True, "all_flags": False, "codemode_vm": True},
    })
    ctx.start()
    ctx.booted()
    ctx.call("config", "GET", "/api/config")
    ctx.stop()
    ctx.make_project(config_extra={"compaction": {"background": "yes"}, "experimental": {"automations": "yes"}})
    ctx.start()
    ctx.booted()
    ctx.call("config.invalid", "GET", "/api/config")


@scenario("api231")
def api231_automation_routes(ctx):
    """the schedule routes are gone; the automation routes decode, check sessions, directories and stored rows, then
    answer 403 while experimental.automations is off; plugin.list names opencode.tool.automation."""
    ctx.set_script([])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session()
    for label, method, path in [
        ("schedule.list", "GET", f"/api/experimental/schedule?sessionID={sid}"),
        ("schedule.create", "POST", "/api/experimental/schedule"),
        ("schedule.get", "GET", "/api/experimental/schedule/sch_ffffffffffffZZZZZZZZZZZZZZ"),
        ("schedule.delete", "DELETE", "/api/experimental/schedule/sch_ffffffffffffZZZZZZZZZZZZZZ"),
    ]:
        ctx.call(label, method, path, {"sessionID": sid, "prompt": "p", "when": {"in": "1h"}} if method == "POST" else None)

    def every(**extra):
        body = {"name": "n", "trigger": {"type": "every", "every": "1 hour"}, "action": {"type": "prompt", "prompt": "p"}}
        body.update(extra)
        return body

    gone = os.path.join(ctx.project, "gone")
    for label, body in [
        ("home", every()),
        ("session", every(sessionID=sid)),
        ("directory", every(directory=ctx.project)),
        ("relative", every(directory="rel")),
        ("missing-directory", every(directory=gone)),
        ("missing-session", every(sessionID=MISSING_SESSION)),
        ("empty", {}),
        ("empty-name", every(name="")),
        ("bad-trigger", {"name": "n", "trigger": {"every": "1h"}, "action": {"prompt": "p"}}),
        ("both-targets", every(action={"type": "prompt", "prompt": "p", "to": {"session": "ses_x", "subagent": "a"}})),
        ("empty-code", every(action={"type": "code", "code": ""})),
        ("subagent", every(sessionID=sid, action={"type": "prompt", "prompt": "p", "to": {"subagent": "general"}})),
    ]:
        ctx.call(f"automation.create.{label}", "POST", "/api/experimental/automation", body)
    for label, query in [("all", ""), ("session", f"?sessionID={sid}"), ("directory", f"?directory={ctx.project}"),
                         ("gone", f"?directory={gone}"), ("relative", "?directory=rel"),
                         ("missing-session", f"?sessionID={MISSING_SESSION}"), ("bad-session", "?sessionID=bad")]:
        ctx.call(f"automation.list.{label}", "GET", f"/api/experimental/automation{query}")
    base = f"/api/experimental/automation/{MISSING_AUTOMATION}"
    ctx.call("automation.get.missing", "GET", base)
    ctx.call("automation.get.bad", "GET", "/api/experimental/automation/bad")
    ctx.call("automation.pause.missing", "POST", f"{base}/pause")
    ctx.call("automation.resume.missing", "POST", f"{base}/resume")
    ctx.call("automation.delete.missing", "DELETE", base)
    ctx.settle("/api/plugin")
    _, plugins = ctx.call("plugin.list", "GET", "/api/plugin", record=False)
    ids = [p.get("id") for p in (plugins.get("data", []) if isinstance(plugins, dict) else [])]
    ctx.rec("plugin.list tool plugins", [i for i in ids if isinstance(i, str) and i.startswith("opencode.tool.")])
    ctx.settle("/api/command")
    _, commands = ctx.call("command.list", "GET", "/api/command", record=False)
    ctx.rec("command.list names", sorted(c.get("name") for c in commands.get("data", [])))


@scenario("api231")
def api231_video_blob(ctx):
    """the video routes decode, then answer 403 while experimental.video_generate is off; blobs in <data>/blobs are
    served with their sniffed type, an immutable ETag, 304, single ranges, a strong If-Range and 416."""
    ctx.set_script([])
    ctx.make_project()
    blobs = os.path.join(ctx.state, "data", "opencode", "blobs")
    os.makedirs(blobs, exist_ok=True)
    with open(os.path.join(blobs, BLOB), "wb") as f:
        f.write(b"\0\0\0\x18ftypisom" + bytes(range(256)))
    with open(os.path.join(blobs, TEXT_BLOB), "wb") as f:
        f.write(b"hello blob")
    ctx.start()
    route = "/api/experimental/generate/video"
    ctx.call("video.create", "POST", route, {"model": {"providerID": "x", "id": "y"}, "prompt": "p"})
    ctx.call("video.create.empty", "POST", route, {})
    ctx.call("video.create.bad-model", "POST", route, {"model": "x/y", "prompt": "p"})
    ctx.call("video.create.bad-aspect", "POST", route, {"model": {"providerID": "x", "id": "y"}, "prompt": "p", "aspect": "bad"})
    ctx.call("video.get", "GET", f"{route}/video_0123456789abABCDEFGHIJKLMN")
    ctx.call("video.get.bad", "GET", f"{route}/bad")
    keep = ["content-type", "content-disposition", "content-security-policy", "x-content-type-options",
            "cache-control", "etag", "accept-ranges", "content-range", "content-length"]
    for label, blob, headers in [
        ("video", BLOB, {}),
        ("text", TEXT_BLOB, {}),
        ("range", TEXT_BLOB, {"range": "bytes=2-5"}),
        ("suffix", TEXT_BLOB, {"range": "bytes=-3"}),
        ("unsatisfiable", TEXT_BLOB, {"range": "bytes=50-"}),
        ("if-range-other", TEXT_BLOB, {"range": "bytes=2-5", "if-range": '"other"'}),
        ("if-range-match", TEXT_BLOB, {"range": "bytes=2-5", "if-range": f'"{TEXT_BLOB}"'}),
        ("not-modified", TEXT_BLOB, {"if-none-match": f'W/"{TEXT_BLOB}"'}),
        ("missing", "video_0123456789abABCDEFGHIJKLMN_2", {}),
        ("bad", "bad", {}),
    ]:
        status, head, body = raw(ctx, f"/api/experimental/blob/{blob}", headers)
        ctx.rec(f"blob.{label}", {"status": status, "headers": {k: head.get(k) for k in keep if head.get(k) is not None},
                                  "body": body.decode("latin-1") if len(body) < 64 else f"<{len(body)} bytes>"})


def raw(ctx, path, headers):
    import urllib.error
    import urllib.request
    req = urllib.request.Request(ctx.server.url + path)
    req.add_header("authorization", ctx.server.auth)
    for k, v in headers.items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, {k.lower(): v for k, v in resp.headers.items()}, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, {k.lower(): v for k, v in e.headers.items()}, e.read()
