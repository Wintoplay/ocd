"""2.3.0 MCP OAuth and provider sign-in: a connection refreshes a token 30 s before it expires with the discovery
its login saved (#63028), sends nothing to an authorization server other than the one that issued the login, needs a
new login for a 403 `insufficient_scope`, a login registers both loopback redirects and redeems the code with the
one the browser came back on (#63403), and provider sign-ins accept the redirect URL pasted from the browser (#63315).

The MCP server and its authorization server are mcp227's local fake. Provider sign-ins never reach their providers:
only pastes that end or keep the attempt before any token request are made."""
import http.client
import json
import time
import urllib.parse

from parity.mcp227 import (NAME, _attempt, _attempt_status, _complete, _credentials, _start, _statuses,
                           _wait_status, fake, integration_id)
from parity.registry import scenario


def _seed(ctx, remote, expires, metadata=None, access="old", refresh="ref-1"):
    iid = integration_id(remote.url)
    meta = {"serverUrl": remote.url, "tokenType": "Bearer", "issuer": remote.base,
            "client": {"client_id": "dyn-client", "issuer": remote.base}}
    meta.update(metadata or {})
    value = {"type": "oauth", "methodID": iid, "access": access, "refresh": refresh, "expires": expires, "metadata": meta}
    ctx.call("credential.create", "POST", "/api/credential", {"integrationID": iid, "label": NAME, "value": value})


def _bearers(remote):
    with remote.lock:
        seen = []
        for b in remote.bearers:
            if b not in seen:
                seen.append(b)
        return seen


@scenario("auth230", timeout=120)
def auth230_oauth_refresh_ahead(ctx):
    """A token that expires within 30 s is refreshed before it is sent, with the discovery the login saved: the server
    never sees the old token. A token with time left goes out as it is and the 401 refreshes it."""
    remote = fake()
    _start(ctx, remote)
    _wait_status(ctx, "needs_auth")
    discovery = {"authorizationServer": remote.base, "resource": remote.url}
    _seed(ctx, remote, int(time.time() * 1000) + 10_000, {"discovery": discovery})
    ctx.rec("connected", _wait_status(ctx, "connected"))
    # Upstream's row has once read back unrefreshed right after connecting (1 run in 4); let writes settle.
    time.sleep(1.5)
    with remote.lock:
        ctx.rec("token.requests", list(remote.token_requests))
    ctx.rec("bearers", _bearers(remote))
    _credentials(ctx, "credentials", remote)


@scenario("auth230", timeout=120)
def auth230_oauth_issuer_mismatch(ctx):
    """Discovery names another authorization server than the one that issued the login: no refresh is sent there, the
    server needs a new login and the login stays."""
    remote = fake()
    _start(ctx, remote)
    _wait_status(ctx, "needs_auth")
    _seed(ctx, remote, 1, {"issuer": "https://auth.legit.example"})
    time.sleep(1.5)
    ctx.rec("status", _wait_status(ctx, "needs_auth"))
    with remote.lock:
        ctx.rec("token.requests", list(remote.token_requests))
    ctx.rec("bearers", _bearers(remote))
    _credentials(ctx, "credentials", remote)


@scenario("auth230", timeout=120)
def auth230_oauth_insufficient_scope(ctx):
    """A 403 `insufficient_scope` needs a new login; a connection does not step up, and the login stays."""
    remote = fake()
    remote.forbid = "admin"
    _start(ctx, remote)
    _wait_status(ctx, "needs_auth")
    with remote.lock:
        remote.valid.add("tok-9")
    _seed(ctx, remote, 0, access="tok-9")
    time.sleep(1.5)
    ctx.rec("status", _wait_status(ctx, "needs_auth"))
    ctx.call("mcp.list", "GET", f"/api/mcp?directory={ctx.project}")
    with remote.lock:
        ctx.rec("token.requests", list(remote.token_requests))
        ctx.rec("registrations", len(remote.registrations))
    _credentials(ctx, "credentials", remote)


@scenario("auth230", timeout=120)
def auth230_oauth_localhost_redirect(ctx):
    """Both loopback redirects are registered; the browser comes back on localhost (an authorization server rewrote
    127.0.0.1), so the code is redeemed with the localhost redirect. The login saves its discovery."""
    remote = fake()
    _start(ctx, remote)
    _wait_status(ctx, "needs_auth")
    iid = integration_id(remote.url)
    attempt, redirect, state = _attempt(ctx, "connect.oauth", iid)
    target = urllib.parse.urlparse(redirect)
    port = target.port

    def masked(uri):
        return uri.replace(f":{port}/", ":<port>/")

    with remote.lock:
        ctx.rec("registration.redirect_uris", [masked(u) for u in remote.registrations[0].get("redirect_uris", [])])
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
    conn.request("GET", f"{target.path}?code=abc&state={urllib.parse.quote(state)}",
                 headers={"Host": f"localhost:{port}"})
    response = conn.getresponse()
    ctx.rec("callback.status", response.status)
    response.read()
    conn.close()
    _attempt_status(ctx, "attempt.done", iid, attempt)
    ctx.rec("mcp", _wait_status(ctx, "connected"))
    with remote.lock:
        ctx.rec("token.redirects", [masked(u) for u in remote.redirects])
    _, data = ctx.call("credentials", "GET", "/api/credential", record=False)
    rows = [c for c in (data or {}).get("data", []) if c.get("integrationID") == iid]
    ctx.rec("credentials", json.loads(masked(json.dumps(rows))))


def _provider_attempt(ctx, label, iid, method, answer=None):
    body = {"methodID": method}
    if answer is not None:
        body["answer"] = answer
    status, data = ctx.call(label, "POST", f"/api/integration/{iid}/connect/oauth", body, record=False)
    attempt = (data or {}).get("data", data) if isinstance(data, dict) else {}
    if not isinstance(attempt, dict) or "url" not in attempt:
        ctx.rec(label, {"status": status, "body": data})
        return None, "", ""
    url = urllib.parse.urlparse(attempt.get("url", ""))
    query = urllib.parse.parse_qs(url.query)
    redirect = query.get("redirect_uri", [""])[0]
    target = urllib.parse.urlparse(redirect)
    shown = dict(attempt)
    shown["url"] = {"endpoint": f"{url.scheme}://{url.netloc}{url.path}", "params": sorted(query),
                    "redirect": f"{target.scheme}://{target.hostname}<port>{target.path}"}
    ctx.rec(label, {"status": status, "body": shown})
    return attempt, redirect, query.get("state", [""])[0]


@scenario("auth230", timeout=180)
def auth230_provider_pasted_redirect(ctx):
    """Provider browser sign-ins say `paste: true` and check a pasted redirect URL as they check the browser's: a
    wrong paste keeps the attempt waiting with the reason, an error in the URL ends it (Poe also checks the issuer and
    hides an error that echoes the state). Device-code sign-ins are not started: they would reach their providers."""
    ctx.set_script([])
    ctx.make_project()
    ctx.start()
    ctx.create_session()
    ctx.call("integration.openai", "GET", "/api/integration/openai")
    signins = [
        ("poe", "browser", None),
        ("snowflake-cortex", "browser", {"account": "myorg-acct"}),
        ("openai", "chatgpt-browser", None),
        ("openai", "chatgpt-token-sharing", None),
    ]
    for iid, method, answer in signins:
        label = f"{iid}.{method}"
        attempt, redirect, state = _provider_attempt(ctx, f"{label}.connect", iid, method, answer)
        if not attempt:
            continue
        aid = attempt["attemptID"]
        _complete(ctx, f"{label}.paste.not_a_url", iid, attempt, "abc")
        _complete(ctx, f"{label}.paste.other_state", iid, attempt, f"{redirect}?code=abc&state=nope")
        _complete(ctx, f"{label}.paste.no_code", iid, attempt, f"{redirect}?state={state}")
        ctx.call(f"{label}.still_pending", "GET", f"/api/integration/{iid}/connect/oauth/{aid}")
        if iid == "poe":
            _complete(ctx, f"{label}.paste.issuer", iid, attempt, f"{redirect}?code=abc&state={state}&iss=https://evil.example")
            _attempt_status(ctx, f"{label}.failed.issuer", iid, attempt)
            attempt, redirect, state = _provider_attempt(ctx, f"{label}.connect.again", iid, method, answer)
            _complete(ctx, f"{label}.paste.echo", iid, attempt,
                      f"{redirect}?error=bad&error_description=state%20{state}%20rejected&state={state}")
            _attempt_status(ctx, f"{label}.failed.echo", iid, attempt)
            continue
        _complete(ctx, f"{label}.paste.error", iid, attempt,
                  f"{redirect}?error=access_denied&error_description=User%20said%20no&state={state}")
        _attempt_status(ctx, f"{label}.failed", iid, attempt)
