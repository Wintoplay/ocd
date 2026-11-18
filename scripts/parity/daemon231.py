"""opencode 2.3.1 shell commands in the process daemon: a command outlives a restart handoff (`service restart`), the
next server settles the call its predecessor left unsettled as moved to the background, watches the command and
reports when it ends."""
import json
import os
import time

from parity.registry import scenario
from parity.session225 import _service_password


def call(name, args, id):
    return {"name": name, "args": args, "id": id}


def _restart(ctx, sid, script_turns):
    """Prepare a handoff on the running server, stop it, and start the managed service with the handoff."""
    _, prepared = ctx.call("handoff", "POST", "/api/experimental/persistent-pty/handoff", record=False)
    handoff = (prepared or {}).get("data", prepared or {}).get("handoff") if isinstance(prepared, dict) else None
    ctx.rec("handoff.prepared", handoff is not None)
    ctx.record_events("before", filt=lambda e: e.get("type", "").startswith("shell."))
    ctx.stop()
    _service_password(ctx)
    ctx.env_extra = dict(ctx.env_extra or {}, OPENCODE_PTY_HANDOFF=json.dumps(handoff) if handoff else "")
    port = 47000 + (os.getpid() % 1000)
    ctx.start(["--service", "--port", str(port), "--hostname", "127.0.0.1"])
    deadline = time.time() + 60
    while time.time() < deadline and len(ctx.mock.main_requests()) < script_turns:
        time.sleep(0.2)
    time.sleep(2)


@scenario("daemon231", hold_for_title=0, timeout=240)
def daemon231_restart_foreground_shell(ctx):
    """A foreground shell call still running when the server is replaced through the handoff: the command keeps
    running in the daemon, the next server settles the call as moved to the background, resumes the session, and the
    command's completion notice (with its whole output) wakes it again."""
    ctx.set_script([
        {"tools": [call("shell", {"command": "echo before; sleep 12; echo after"}, "call_long")]},
        {"text": "Waiting for it."},
        {"text": "It finished."},
    ])
    ctx.make_project(config_extra={"permission": {"shell": "allow"}})
    ctx.start()
    sid = ctx.create_session(title="Daemon")
    ctx.call("prompt", "POST", f"/api/session/{sid}/prompt", {"text": "run it"})
    ctx.wait_for("shell.created", timeout=20)
    time.sleep(0.5)
    _restart(ctx, sid, 3)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events("after", filt=lambda e: e.get("type", "").startswith(("shell.", "session.tool.", "session.synthetic")))
    ctx.record_llm()
    ctx.stop()


@scenario("daemon231", hold_for_title=0, timeout=240)
def daemon231_restart_background_shell(ctx):
    """A background shell job running when the server is replaced through the handoff: the next server watches the
    job again, and its completion notice resumes the session."""
    ctx.set_script([
        {"tools": [call("shell", {"command": "echo started; sleep 4; echo ended", "background": True}, "call_bg")]},
        {"text": "Started it."},
        {"text": "Noted."},
    ])
    ctx.make_project(config_extra={"permission": {"shell": "allow"}})
    ctx.start()
    sid = ctx.create_session(title="Daemon")
    ctx.call("prompt", "POST", f"/api/session/{sid}/prompt", {"text": "start it"})
    deadline = time.time() + 20
    while time.time() < deadline and len(ctx.mock.main_requests()) < 2:
        time.sleep(0.2)
    time.sleep(1)
    _restart(ctx, sid, 3)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events("after", filt=lambda e: e.get("type", "").startswith(("shell.", "session.synthetic")))
    ctx.record_llm()
    ctx.stop()
