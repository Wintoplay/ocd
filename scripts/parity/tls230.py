"""2.3.0 provider client certificates (`settings.tls`, #61832) against a local HTTPS server that requires one.

The CA, server and client certificates and keys are made with the `openssl` CLI once per suite run, in a temporary
directory under $OCD_SCRATCH (so both binaries read the same files), and the server's CA is trusted through
`NODE_EXTRA_CA_CERTS`. Every call goes through `POST /api/experimental/generate/{text,decisions}` at the global
location, one provider per case: what each call answers, and which requests reached the server with which client
certificate (its common name)."""
import json
import os
import socket
import ssl
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from parity.image import boot_global
from parity.plugins import AISDK_PROVIDER, BUN
from parity.registry import scenario

_PKI = {}


def _openssl(*args, cwd):
    subprocess.run(["openssl", *args], cwd=cwd, check=True, capture_output=True)


def pki():
    """The certificate files (generated once): path by name."""
    if _PKI:
        return _PKI
    root = tempfile.mkdtemp(prefix="tls230-", dir=os.environ.get("OCD_SCRATCH") or None)
    o = lambda *a: _openssl(*a, cwd=root)  # noqa: E731
    o("req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "ca.key",
      "-out", "ca.pem", "-days", "30", "-subj", "/CN=Parity CA", "-addext", "basicConstraints=critical,CA:TRUE",
      "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    with open(os.path.join(root, "server.ext"), "w") as f:
        f.write("subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n")
    with open(os.path.join(root, "client.ext"), "w") as f:
        f.write("basicConstraints=CA:FALSE\nextendedKeyUsage=clientAuth\n")

    def leaf(name, cn, newkey, ext):
        o("req", "-newkey", *newkey, "-nodes", "-keyout", f"{name}.key", "-out", f"{name}.csr", "-subj", f"/CN={cn}")
        o("x509", "-req", "-in", f"{name}.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
          "-out", f"{name}.pem", "-days", "30", "-extfile", ext)

    leaf("server", "localhost", ["ec", "-pkeyopt", "ec_paramgen_curve:P-256"], "server.ext")
    leaf("client", "rsa-client", ["rsa:2048"], "client.ext")
    leaf("p384", "p384-client", ["ec", "-pkeyopt", "ec_paramgen_curve:P-384"], "client.ext")
    o("genpkey", "-algorithm", "ed25519", "-out", "ed25519.key")
    o("req", "-new", "-key", "ed25519.key", "-out", "ed25519.csr", "-subj", "/CN=ed25519-client")
    o("x509", "-req", "-in", "ed25519.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
      "-out", "ed25519.pem", "-days", "30", "-extfile", "client.ext")
    o("pkcs8", "-topk8", "-in", "client.key", "-out", "pkcs8-aes.key", "-passout", "pass:secret", "-v2", "aes-256-cbc")
    o("pkcs8", "-topk8", "-in", "client.key", "-out", "pkcs8-3des.key", "-passout", "pass:secret", "-v2", "des3")
    o("pkcs8", "-topk8", "-in", "client.key", "-out", "pkcs8-v1.key", "-passout", "pass:secret", "-v1", "PBE-SHA1-3DES")
    o("pkcs8", "-topk8", "-in", "client.key", "-out", "pkcs8-scrypt.key", "-passout", "pass:secret", "-scrypt")
    o("rsa", "-in", "client.key", "-out", "legacy-aes.key", "-aes256", "-passout", "pass:secret", "-traditional")
    o("rsa", "-in", "client.key", "-out", "legacy-3des.key", "-des3", "-passout", "pass:secret", "-traditional")
    o("rsa", "-in", "client.key", "-out", "pkcs1.key", "-traditional")
    o("ec", "-in", "p384.key", "-out", "p384-sec1.key")
    with open(os.path.join(root, "chain.pem"), "w") as f:
        f.write(open(os.path.join(root, "client.pem")).read() + open(os.path.join(root, "ca.pem")).read())
    for name in os.listdir(root):
        _PKI[name] = os.path.join(root, name)
    _PKI["missing.key"] = os.path.join(root, "missing.key")
    return _PKI


_PORT = {}


class MtlsServer:
    """An OpenAI-compatible HTTPS server that requires a client certificate signed by the CA."""

    def __init__(self):
        files = pki()
        self.requests = []
        self.lock = threading.Lock()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _send(self, status, body, ctype="application/json", extra=None):
                data = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("content-type", ctype)
                self.send_header("content-length", str(len(data)))
                for k, v in (extra or {}).items():
                    self.send_header(k, v)
                self.end_headers()
                self.wfile.write(data)

            def do_POST(self):
                cert = self.connection.getpeercert() or {}
                cn = next((v for rdn in cert.get("subject", ()) for k, v in rdn if k == "commonName"), None)
                n = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(n) if n else b""
                try:
                    body = json.loads(raw)
                except Exception:
                    body = None
                with outer.lock:
                    outer.requests.append({"path": self.path, "cn": cn,
                                           "model": body.get("model") if isinstance(body, dict) else None})
                if "/redirect/" in self.path:
                    port = self.server.server_address[1]
                    return self._send(307, b"", "text/plain", {"location": f"https://127.0.0.1:{port}/v1/chat/completions"})
                if self.path.endswith("/systemone"):
                    return self._send(200, {"model": body.get("model"), "answers": {"refund": {"type": "noul", "noul": 0.9}},
                                            "usage": {"input_tokens": 9, "output_tokens": 1}})
                if self.path.endswith("/chat/completions"):
                    text = f"certified answer for {cn}"
                    if isinstance(body, dict) and body.get("stream"):
                        chunks = [
                            {"id": "c", "object": "chat.completion.chunk", "created": 1, "model": body.get("model"),
                             "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]},
                            {"id": "c", "object": "chat.completion.chunk", "created": 1, "model": body.get("model"),
                             "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                             "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}},
                        ]
                        data = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
                        return self._send(200, data.encode(), "text/event-stream")
                    return self._send(200, {"id": "c", "object": "chat.completion", "created": 1, "model": body.get("model"),
                                            "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                                                         "finish_reason": "stop"}],
                                            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})
                return self._send(404, {"error": "not found"})

        if "port" not in _PORT:
            s = socket.socket()
            s.bind(("127.0.0.1", 0))
            _PORT["port"] = s.getsockname()[1]
            s.close()
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(files["server.pem"], files["server.key"])
        context.load_verify_locations(files["ca.pem"])
        context.verify_mode = ssl.CERT_REQUIRED
        ThreadingHTTPServer.allow_reuse_address = True
        self.httpd = ThreadingHTTPServer(("127.0.0.1", _PORT["port"]), Handler)
        self.httpd.daemon_threads = True
        self.httpd.socket = context.wrap_socket(self.httpd.socket, server_side=True)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.origin = f"https://127.0.0.1:{_PORT['port']}"

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()

    def seen(self):
        with self.lock:
            return list(self.requests)

    def clear(self):
        with self.lock:
            self.requests.clear()


def tls(cert, key, passphrase=None):
    files = pki()
    out = {"cert": "{file:" + files[cert] + "}", "key": "{file:" + files[key] + "}"}
    if passphrase is not None:
        out["passphrase"] = passphrase
    return out


def corp(base, tls_value=None, package="@opencode/ai/providers/openai-compatible", models=None, extra=None):
    settings = {"baseURL": base, "apiKey": "k"}
    if tls_value is not None:
        settings["tls"] = tls_value
    settings.update(extra or {})
    return {"name": "Corp", "package": package, "settings": settings,
            "models": models or {"m1": {"name": "M1", "limit": {"context": 100000, "output": 4096}}}}


def start(ctx, providers):
    ctx.env_extra["NODE_EXTRA_CA_CERTS"] = pki()["ca.pem"]
    boot_global(ctx, {"model": "mock/m1", "providers": providers})


def text(ctx, label, provider, model="m1"):
    ctx.call(label, "POST", "/api/experimental/generate/text",
             {"prompt": "hi", "model": {"providerID": provider, "id": model}})


@scenario("tls230", timeout=180)
def tls230_chat(ctx):
    """a provider with `tls` presents the client certificate; without it (or with a `{file:}` that doesn't exist) the
    server refuses the handshake; a plain HTTP endpoint doesn't need a usable certificate."""
    server = MtlsServer()
    try:
        base = server.origin + "/v1"
        start(ctx, {
            "certified": corp(base, tls("client.pem", "client.key")),
            "uncertified": corp(base),
            "missing": corp(base, tls("client.pem", "missing.key")),
            "plain": corp(f"http://127.0.0.1:{ctx.mock.port}/v1", {"cert": "garbage", "key": "garbage"}),
        })
        ctx.set_script([{"text": "plain answer"}])
        for name in ("certified", "uncertified", "missing", "plain"):
            text(ctx, name, name)
        ctx.rec("server", server.seen())
    finally:
        server.close()


@scenario("tls230", timeout=180)
def tls230_bad_keys(ctx):
    """keys that can't be used: another certificate's key, a certificate instead of a key, a wrong or missing
    passphrase, a scrypt-encrypted PKCS#8 key; and a redirect, which a certified request refuses."""
    server = MtlsServer()
    try:
        base = server.origin + "/v1"
        start(ctx, {
            "mismatch": corp(base, tls("client.pem", "p384.key")),
            "notakey": corp(base, tls("client.pem", "ca.pem")),
            "wrongpass": corp(base, tls("client.pem", "pkcs8-aes.key", "nope")),
            "nopass": corp(base, tls("client.pem", "pkcs8-aes.key")),
            "scrypt": corp(base, tls("client.pem", "pkcs8-scrypt.key", "secret")),
            "redirect": corp(server.origin + "/redirect/v1", tls("client.pem", "client.key")),
        })
        for name in ("mismatch", "notakey", "wrongpass", "nopass", "scrypt", "redirect"):
            text(ctx, name, name)
        ctx.rec("server", server.seen())
    finally:
        server.close()


@scenario("tls230", timeout=180)
def tls230_key_formats(ctx):
    """key formats: PKCS#1, encrypted PKCS#8 (PBES2 AES and 3DES, PKCS#12 3DES), legacy OpenSSL-encrypted PEM (AES,
    3DES), SEC1 P-384, Ed25519, a certificate followed by its chain, and a passphrase for an unencrypted key."""
    server = MtlsServer()
    try:
        base = server.origin + "/v1"
        cases = {
            "pkcs1": tls("client.pem", "pkcs1.key"),
            "pkcs8aes": tls("client.pem", "pkcs8-aes.key", "secret"),
            "pkcs83des": tls("client.pem", "pkcs8-3des.key", "secret"),
            "pkcs12": tls("client.pem", "pkcs8-v1.key", "secret"),
            "legacyaes": tls("client.pem", "legacy-aes.key", "secret"),
            "legacy3des": tls("client.pem", "legacy-3des.key", "secret"),
            "p384": tls("p384.pem", "p384-sec1.key"),
            "ed25519": tls("ed25519.pem", "ed25519.key"),
            "chain": tls("chain.pem", "client.key"),
            "extrapass": tls("client.pem", "client.key", "unused"),
        }
        start(ctx, {name: corp(base, value) for name, value in cases.items()})
        for name in cases:
            text(ctx, name, name)
        ctx.rec("server", server.seen())
    finally:
        server.close()


@scenario("tls230", timeout=180)
def tls230_decisions(ctx):
    """a decision model of a provider with `tls` presents the certificate on its System One request."""
    server = MtlsServer()
    try:
        base = server.origin + "/v1"
        models = {"decider": {"name": "Decider", "type": "decision"}}
        package = "@opencode/ai/providers/openai-compatible/decisions"
        start(ctx, {
            "certified": corp(base, tls("client.pem", "client.key"), package, models),
            "uncertified": corp(base, None, package, models),
        })
        for name in ("certified", "uncertified"):
            ctx.call(name, "POST", "/api/experimental/generate/decisions", {
                "model": {"providerID": name, "id": "decider"},
                "input": [{"type": "text", "text": "Please refund the duplicate charge."}],
                "questions": [{"name": "refund", "type": "predicate", "instructions": "Refund asked?"}]})
        ctx.rec("server", server.seen())
    finally:
        server.close()


@scenario("tls230", timeout=240)
def tls230_aisdk(ctx):
    """an AI SDK provider package (the fixture, through the SDK's fetch) presents the certificate too, and refuses a
    redirect."""
    server = MtlsServer()
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    try:
        start(ctx, {
            "certified": corp(server.origin + "/v1", tls("client.pem", "client.key"), AISDK_PROVIDER),
            "uncertified": corp(server.origin + "/v1", None, AISDK_PROVIDER),
            "redirect": corp(server.origin + "/redirect/v1", tls("client.pem", "client.key"), AISDK_PROVIDER),
        })
        for name in ("certified", "uncertified", "redirect"):
            text(ctx, name, name)
        ctx.rec("server", server.seen())
    finally:
        server.close()


@scenario("tls230", hold_for_title=0, timeout=240)
def tls230_session(ctx):
    """sessions on providers with `tls`: the answer, and for a key that can't be used, an endpoint that redirects
    and a provider without the certificate the first failure (retried as a transport error), then an interrupt."""
    server = MtlsServer()
    try:
        base = server.origin + "/v1"
        ctx.env_extra["NODE_EXTRA_CA_CERTS"] = pki()["ca.pem"]
        ctx.set_script([])
        ctx.make_project(config={"model": "certified/m1", "small_model": "certified/m1", "providers": {
            "certified": corp(base, tls("client.pem", "client.key")),
            "notakey": corp(base, tls("client.pem", "ca.pem")),
            "redirect": corp(server.origin + "/redirect/v1", tls("client.pem", "client.key")),
            "uncertified": corp(base),
        }})
        ctx.start()
        for name in ("certified", "notakey", "redirect", "uncertified"):
            sid = ctx.create_session(label=f"{name}.session", title=name, model={"providerID": name, "id": "m1"})
            mark = len(ctx.server.events)
            if name == "certified":
                ctx.prompt(sid, "hello", label=f"{name}.prompt")
            else:
                ctx.prompt(sid, "hello", label=f"{name}.prompt", wait=False)
                event = ctx.wait_for("session.retry.scheduled", start=mark, timeout=20) or \
                    ctx.wait_for("session.step.failed", start=mark, timeout=5)
                data = (event or {}).get("data") or {}
                ctx.rec(f"{name}.failure", {"type": (event or {}).get("type"), "error": data.get("error"),
                                            "attempt": data.get("attempt")})
                ctx.call(f"{name}.interrupt", "POST", f"/api/session/{sid}/interrupt")
                ctx.wait_idle(sid, start=mark)
            ctx.call(f"{name}.messages", "GET", f"/api/session/{sid}/message")
        seen = sorted({json.dumps(r, sort_keys=True) for r in server.seen()})
        ctx.rec("server", [json.loads(r) for r in seen])
    finally:
        server.close()
