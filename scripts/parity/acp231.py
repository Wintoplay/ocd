"""`opencode acp` over stdio, 2.3.1 (#64262, `translate.ts` `shells`/`pending`, `turn.ts` `followBackground`): a prompt
whose turn ends while a background shell still runs hands off like one with a running subagent, and the turn the
shell's notice wakes after the response is streamed to the client. A shell that is done within its turn streams its
reaction too (in that turn or in one it wakes: a race in both binaries, so only whether it arrived is recorded), and
the session takes the next prompt as usual. The shell outputs (`SLOW-2`, `FAST-2`) are computed, so the command text
in the history never matches the mock's turns."""
import os

from parity.acp227 import Acp, CHILD_UPDATES, transcript
from parity.registry import scenario

SLOW = "while [ ! -f release ]; do sleep 0.05; done; echo SLOW-$((1+1))"
FAST = "echo FAST-$((1+1))"


def chunk(text):
    def pred(m):
        update = (m.get("params") or {}).get("update") or {}
        return update.get("sessionUpdate") == "agent_message_chunk" and text in str(update.get("content"))
    return pred


@scenario("acp")
def acp_background_shell_wakes_turn(ctx):
    """a prompt ends with its background shell still running; once the shell ends, the turn its notice wakes is
    streamed; a fast shell's reaction streams too and a follow-up prompt is answered."""
    ctx.set_script([
        {"match": "SLOW-2", "text": "slow shell reaction"},
        {"match": "FAST-2", "text": "fast shell reaction"},
        {"match": "FOLLOW-UP-PROMPT", "text": "follow-up reply"},
        {"match": "run the slow shell", "tools": [{"name": "shell", "id": "call_slow", "args": {
            "command": SLOW, "description": "Slow", "background": True}}]},
        {"match": "run the fast shell", "tools": [{"name": "shell", "id": "call_fast", "args": {
            "command": FAST, "description": "Fast", "background": True}}]},
        {"match": "Output is streaming to", "text": "launched one"},
        {"match": "Output is streaming to", "text": "launched two"},
    ])
    ctx.make_project()
    acp = Acp(ctx)
    acp.request("initialize", {"protocolVersion": 1, "clientCapabilities": CHILD_UPDATES})
    sid = acp.request("session/new", {"cwd": ctx.project, "mcpServers": []})["result"]["sessionId"]
    acp.wait(lambda m: (m.get("params") or {}).get("update", {}).get("sessionUpdate") == "available_commands_update")

    def prompt(text):
        rid = acp.start("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": text}]})
        ctx.rec(f"prompt.{text}", (acp.response(rid) or {}).get("result"))
        return rid

    rid = prompt("run the slow shell")
    acp.wait(chunk("launched one"))
    acp.quiet(gap=1.5)
    ctx.rec("slow.reaction_before_release", acp.wait(chunk("slow shell reaction"), timeout=0.1) is not None)
    with open(os.path.join(ctx.project, "release"), "w"):
        pass
    ctx.rec("slow.reaction", acp.wait(chunk("slow shell reaction"), timeout=30) is not None)
    acp.quiet(gap=2.0)
    transcript(ctx, acp, rid, "acp.slow")
    ctx.rec("llm.slow", [[m for m in r["body"].get("messages", []) if m.get("role") != "system"]
                         for r in ctx.mock.main_requests()])

    prompt("run the fast shell")
    ctx.rec("fast.reaction", acp.wait(chunk("fast shell reaction"), timeout=30) is not None)
    acp.quiet(gap=1.5)
    prompt("FOLLOW-UP-PROMPT")
    ctx.rec("follow_up.reply", acp.wait(chunk("follow-up reply"), timeout=30) is not None)
    acp.quiet(gap=1.0)
    ctx.rec("reactions", {text: sum(1 for m in acp.messages if chunk(text)(m))
                          for text in ("slow shell reaction", "fast shell reaction", "follow-up reply")})
    ctx.rec("acp.exit", acp.close())
