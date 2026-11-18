"""opencode 2.3.1 provider and server changes: reasoning replay on OpenAI Chat routes (#64243: a declared reasoning
field other than `reasoning_details` drops earlier turns' structured details, `reasoning_details` keeps the array), the
ChatGPT token-sharing catalog read from the stored credential (#64470) with the GPT 6.2 ids (#64508), video models kept
in the catalog, and readable 500s for storage failures (#64022)."""
import copy
import json
import os
import sqlite3
import time

from parity.artifact import database
from parity.registry import scenario

SNAPSHOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "assets", "models-dev", "snapshot.txt")

DETAILS = [{"type": "reasoning.text", "text": "Thinking it over.", "signature": "sig-1", "format": "anthropic-claude-v1",
            "index": 0}]


def _chunk(delta, finish=None):
    return {"id": "gen-1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


def _kimi_turn(text):
    """A Kimi-style answer through an OpenAI-compatible gateway: `reasoning_content` and structured details."""
    return {"raw": [
        _chunk({"role": "assistant", "reasoning_content": "Thinking it over.", "reasoning_details": DETAILS}),
        _chunk({"content": text}),
        _chunk({}, "stop"),
        {"id": "gen-1", "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}},
        "[DONE]",
    ]}


def _reasoning_turn(text):
    """An OpenRouter answer: scalar `reasoning` plus the signed `reasoning_details` array, then text."""
    return {"raw": [
        _chunk({"role": "assistant", "reasoning": "Thinking it over.", "reasoning_details": DETAILS}),
        _chunk({"content": text}),
        _chunk({}, "stop"),
        {"id": "gen-1", "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}},
        "[DONE]",
    ]}


@scenario("providers231", hold_for_title=0)
def providers231_reasoning_replay(ctx):
    """An OpenRouter route at the mock answers with scalar reasoning and signed reasoning details. One session per
    model replays that turn: a model that declares no reasoning field sends the text under the observed field and the
    details; one that declares `reasoning_content` sends only the text there (no details); one that declares
    `reasoning_details` sends the array only (never the text over it). Another session switches models mid-way:
    reasoning another model produced is not replayed. Then the same through an OpenAI-compatible gateway whose
    Kimi-style answers carry `reasoning_content` and structured details, on models declaring `reasoning_content`,
    `reasoning_details` and nothing."""
    port = ctx.mock.port
    ids = ["plain", "content", "details"]
    script = []
    for model_id in ids:
        script += [_reasoning_turn(f"First {model_id}."), {"text": f"Second {model_id}."}]
    script += [_reasoning_turn("First switch."), {"text": "Second switch."}]
    gateway = ["kimi", "kimi-details", "kimi-plain"]
    for model_id in gateway:
        script += [_kimi_turn(f"First {model_id}."), {"text": f"Second {model_id}."}]
    ctx.set_script(script)
    model = {"limit": {"context": 100000, "output": 4096}}
    ctx.make_project(config={"model": "orouter/plain", "providers": {"orouter": {
        "name": "Router", "package": "@opencode/ai/providers/openrouter",
        "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test"},
        "models": {
            "plain": {"name": "Plain", **model},
            "content": {"name": "Content", **model, "compatibility": {"reasoningField": "reasoning_content"}},
            "details": {"name": "Details", **model, "compatibility": {"reasoningField": "reasoning_details"}},
        }}, "gateway": {
        "name": "Gateway", "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test"},
        "models": {
            "kimi": {"name": "Kimi", **model, "compatibility": {"reasoningField": "reasoning_content"}},
            "kimi-details": {"name": "Kimi Details", **model, "compatibility": {"reasoningField": "reasoning_details"}},
            "kimi-plain": {"name": "Kimi Plain", **model},
        }}}})
    ctx.start()
    for model_id in ids:
        sid = ctx.create_session(label=f"session.create.{model_id}", title=f"Replay {model_id}",
                                 model={"providerID": "orouter", "id": model_id})
        ctx.prompt(sid, "one", label=f"session.prompt.{model_id}.one")
        ctx.prompt(sid, "two", label=f"session.prompt.{model_id}.two")
        ctx.call(f"messages.{model_id}", "GET", f"/api/session/{sid}/message")
    sid = ctx.create_session(label="session.create.switch", title="Replay switch")
    ctx.prompt(sid, "one", label="session.prompt.switch.one")
    ctx.call("session.model.switch", "POST", f"/api/session/{sid}/model", {"model": {"providerID": "orouter", "id": "details"}})
    ctx.prompt(sid, "two", label="session.prompt.switch.two")
    for model_id in gateway:
        sid = ctx.create_session(label=f"session.create.{model_id}", title=f"Replay {model_id}",
                                 model={"providerID": "gateway", "id": model_id})
        ctx.prompt(sid, "one", label=f"session.prompt.{model_id}.one")
        ctx.prompt(sid, "two", label=f"session.prompt.{model_id}.two")
    ctx.record_llm()
    replays = []
    for r in ctx.mock.main_requests():
        body = r["body"] if isinstance(r["body"], dict) else {}
        replays.append({"model": body.get("model"),
                        "assistant": [m for m in body.get("messages", []) if m.get("role") == "assistant"]})
    ctx.rec("replayed.assistant", replays)


def _catalog():
    """A models.dev catalog: OpenAI's GPT 6.1/6.2 Sol and Astra (with their fast modes), models outside the ChatGPT
    fallback list, and xAI with a chat and a video model."""
    with open(SNAPSHOT) as f:
        snapshot = json.load(f)
    openai = copy.deepcopy(snapshot["openai"])
    base = openai["models"]["gpt-6.1-sol"]
    models = {}
    for mid, name in [("gpt-6.1-sol", "GPT-6.1 Sol"), ("gpt-6.2-sol", "GPT-6.2 Sol"), ("gpt-6.2-astra", "GPT-6.2 Astra"),
                      ("gpt-6.2-terra", "GPT-6.2 Terra"), ("gpt-5.4", "GPT-5.4")]:
        m = copy.deepcopy(base)
        m["id"], m["name"] = mid, name
        models[mid] = m
    openai["models"] = models
    xai = copy.deepcopy(snapshot["xai"])
    chat_id = sorted(k for k, v in xai["models"].items() if v.get("type", "chat") == "chat")[0]
    video = {"id": "grok-imagine-video", "type": "video", "name": "Grok Imagine Video", "release_date": "2026-10-01",
             "modalities": {"input": ["text", "image/png"], "output": ["video"]},
             "capabilities": {"tools": {"supported": False}, "reasoning": {"supported": False}},
             "limit": {"context": 4096, "output": 0},
             "api": {"videos": {"base_url": "https://api.x.ai/v1", "sdk": {"opencodeai": "@opencode/ai/providers/xai/videos"}}}}
    xai["models"] = {chat_id: xai["models"][chat_id], video["id"]: video}
    return {"openai": openai, "xai": xai}


def _models(ctx, provider, query=""):
    _, _, data = ctx.server.request("GET", f"/api/model{query}", headers={"x-opencode-directory": ctx.project})
    rows = [m for m in (data or {}).get("data", []) if m.get("providerID") == provider]
    return sorted(rows, key=lambda m: m["id"])


@scenario("providers231", hold_for_title=0, timeout=180)
def providers231_chatgpt_models(ctx):
    """A ChatGPT token-sharing login whose token expired and can't be refreshed (no registered client ID): the
    catalog keeps its safeguards (HTTP transport, summary compaction, no effort updates) and offers only the fallback
    models the catalog has, GPT 6.2 Sol and Astra included. Video models are listed by type only, and a video model's
    ref sent to image generation is a 422."""
    path = os.path.join(ctx.state, "models-catalog.json")
    os.makedirs(ctx.state, exist_ok=True)
    ctx.set_script([])
    ctx.make_project()
    with open(path, "w") as f:
        json.dump(_catalog(), f)
    ctx.env_extra["OPENCODE_MODELS_PATH"] = path
    ctx.start()
    ctx.booted()
    ctx.rec("models.openai.before", [m["id"] for m in _models(ctx, "openai")])
    ctx.rec("models.xai.chat", [m["id"] for m in _models(ctx, "xai")])
    ctx.rec("models.xai.video", _models(ctx, "xai", "?types=video"))
    ctx.call("generate.image.video_model", "POST", "/api/experimental/generate/image",
             {"prompt": "a cat", "model": {"providerID": "xai", "id": "grok-imagine-video"}})
    status, _ = ctx.call("credential.create", "POST", "/api/credential", {
        "integrationID": "openai",
        "value": {"type": "oauth", "methodID": "chatgpt-token-sharing", "refresh": "parity-refresh",
                  "access": "parity-stale", "expires": 1}}, record=False)
    ctx.rec("credential.create", status)
    deadline = time.time() + 30
    rows = []
    while time.time() < deadline:
        rows = _models(ctx, "openai")
        if rows and all(m["id"] != "gpt-5.4" for m in rows):
            break
        time.sleep(0.2)
    ctx.rec("models.openai.sharing", rows)
    _, _, provider = ctx.server.request("GET", "/api/provider/openai", headers={"x-opencode-directory": ctx.project})
    ctx.rec("provider.openai.sharing", provider)


@scenario("providers231", hold_for_title=0, timeout=120)
def providers231_storage_busy(ctx):
    """Another process holds the database's write lock past the busy timeout: a write answers 500 `UnknownError`
    "Storage is in use by another process" with a ref, and the server works again once the lock is gone."""
    ctx.set_script([])
    ctx.make_project()
    ctx.start()
    ctx.create_session(title="Before")
    db = database(ctx)
    con = sqlite3.connect(db, timeout=30, isolation_level=None)
    try:
        con.execute("BEGIN EXCLUSIVE")
        ctx.call("session.create.locked", "POST", "/api/session", {"title": "Locked", "location": {"directory": ctx.project}})
    finally:
        con.execute("ROLLBACK")
        con.close()
    status, body = ctx.call("session.create.after", "POST", "/api/session", {"title": "After", "location": {"directory": ctx.project}},
                            record=False)
    ctx.rec("session.create.after", {"status": status, "title": (body or {}).get("data", {}).get("title")})
