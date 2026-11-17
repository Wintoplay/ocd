//! Provider client certificates (`settings.tls`, upstream #61832) against a local HTTPS server that requires one. The
//! CA, server and client certificates are generated here; the CA is trusted through `NODE_EXTRA_CA_CERTS`, which
//! the process-wide trust reads on its first handshake.

use std::sync::{Arc, OnceLock};

use futures::StreamExt;
use oc_ai::catalog::resolve::{resolve_media, resolve_model, MediaModel, MediaType};
use oc_ai::catalog::{Activation, ModelInfo, ProviderInfo};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Pki {
    ca: rcgen::Certificate,
    ca_key: rcgen::KeyPair,
    server: (String, String),
    _dir: tempfile::TempDir,
}

/// The CA (trusted by the process), and a server certificate for `127.0.0.1` and `localhost`.
fn pki() -> &'static Pki {
    static PKI: OnceLock<Pki> = OnceLock::new();
    PKI.get_or_init(|| {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(rcgen::DnType::CommonName, "Test CA");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = params.self_signed(&ca_key).unwrap();
        let server_key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
        let server = params.signed_by(&server_key, &ca, &ca_key).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ca.pem");
        std::fs::write(&file, ca.pem()).unwrap();
        // SAFETY: set once, before any client of this test binary handshakes.
        unsafe { std::env::set_var("NODE_EXTRA_CA_CERTS", &file) };
        Pki { ca, ca_key, server: (server.pem(), server_key.serialize_pem()), _dir: dir }
    })
}

/// A client certificate signed by the CA: (cert PEM, PKCS#8 key PEM).
fn client_cert(name: &str) -> (String, String) {
    let pki = pki();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(rcgen::DnType::CommonName, name);
    let cert = params.signed_by(&key, &pki.ca, &pki.ca_key).unwrap();
    (cert.pem(), key.serialize_pem())
}

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    /// The client presented a certificate.
    certified: bool,
}

/// An HTTPS server that requires a client certificate signed by the CA (and an HTTP one when `tls` is false).
struct Server {
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn answer(path: &str, port: u16) -> Vec<u8> {
    let (status, content_type, body, extra) = if path.starts_with("/redirect") {
        ("307 Temporary Redirect", "text/plain", String::new(), format!("location: https://127.0.0.1:{port}/v1/chat/completions\r\n"))
    } else if path.ends_with("/chat/completions") {
        let chunks = [
            json!({"id": "c", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "certified answer"}}]}),
            json!({"id": "c", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}}),
        ];
        let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        ("200 OK", "text/event-stream", body, String::new())
    } else if path.ends_with("/systemone") {
        let body = json!({"model": "decider", "answers": {"refund": {"type": "noul", "noul": 0.9}}}).to_string();
        ("200 OK", "application/json", body, String::new())
    } else {
        ("404 Not Found", "text/plain", String::new(), String::new())
    };
    format!("HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{body}", body.len()).into_bytes()
}

async fn exchange<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(stream: &mut S, port: u16) -> Option<String> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let length: usize = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0);
    while data.len() < header_end + length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();
    stream.write_all(&answer(&path, port)).await.ok()?;
    let _ = stream.shutdown().await;
    Some(path)
}

impl Server {
    async fn start(tls: bool) -> Server {
        let pki = pki();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let recorded = seen.clone();
        let acceptor = tls.then(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut roots = rustls::RootCertStore::empty();
            roots.add(pki.ca.der().clone()).unwrap();
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build().unwrap();
            use rustls::pki_types::pem::PemObject;
            let chain = vec![rustls::pki_types::CertificateDer::from_pem_slice(pki.server.0.as_bytes()).unwrap()];
            let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(pki.server.1.as_bytes()).unwrap();
            let mut config = rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_client_cert_verifier(verifier)
                .with_single_cert(chain, key)
                .unwrap();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            tokio_rustls::TlsAcceptor::from(Arc::new(config))
        });
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else { return };
                let (acceptor, recorded) = (acceptor.clone(), recorded.clone());
                tokio::spawn(async move {
                    match acceptor {
                        Some(acceptor) => {
                            let Ok(mut stream) = acceptor.accept(socket).await else { return };
                            let certified = stream.get_ref().1.peer_certificates().is_some_and(|c| !c.is_empty());
                            if let Some(path) = exchange(&mut stream, port).await {
                                recorded.lock().push(Seen { path, certified });
                            }
                        }
                        None => {
                            let mut socket = socket;
                            if let Some(path) = exchange(&mut socket, port).await {
                                recorded.lock().push(Seen { path, certified: false });
                            }
                        }
                    }
                });
            }
        });
        let scheme = if tls { "https" } else { "http" };
        Server { origin: format!("{scheme}://127.0.0.1:{port}"), seen, task }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }
}

fn provider(base: &str, tls: Option<Value>, extra: Value) -> ProviderInfo {
    let mut provider = ProviderInfo::empty("corp");
    provider.package = "@opencode/ai/providers/openai-compatible".into();
    provider.activation = Activation::Enabled;
    let mut settings = json!({"baseURL": base, "apiKey": "k"});
    if let Some(tls) = tls {
        settings["tls"] = tls;
    }
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        settings[k] = v;
    }
    provider.settings = settings.as_object().cloned();
    provider
}

fn chat_model() -> ModelInfo {
    let mut model = ModelInfo::default_for("corp", "m1");
    model.package = Some("@opencode/ai/providers/openai-compatible".into());
    model
}

async fn chat(provider: &ProviderInfo) -> Result<String, oc_ai::AIError> {
    let resolved = resolve_model(Some(provider), &chat_model(), None, None, &Default::default()).unwrap();
    let request = oc_ai::LLMRequest::new(resolved.language_model).prompt("hi");
    let mut stream = oc_ai::Client::new().stream(request, oc_ai::CancellationToken::new()).await?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        if let oc_ai::LLMEvent::TextDelta { text: delta, .. } = event? {
            text.push_str(&delta);
        }
    }
    Ok(text)
}

fn tls_value(cert: &str, key: &str) -> Value {
    json!({"cert": cert, "key": key})
}

fn transport_code(error: &oc_ai::AIError) -> Option<String> {
    match &error.kind {
        oc_ai::ErrorKind::Transport { code, .. } => code.clone(),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_presents_the_certificate_and_fails_without_it() {
    let server = Server::start(true).await;
    let (cert, key) = client_cert("ocd-client");
    let base = format!("{}/v1", server.origin);
    let text = chat(&provider(&base, Some(tls_value(&cert, &key)), json!({}))).await.unwrap();
    assert_eq!(text, "certified answer");
    assert_eq!(server.seen().len(), 1);
    assert!(server.seen()[0].certified);
    assert_eq!(server.seen()[0].path, "/v1/chat/completions");

    // Without `tls` (or with an empty `{file:}` key) the server refuses the handshake.
    for tls in [None, Some(tls_value(&cert, ""))] {
        let error = chat(&provider(&base, tls, json!({}))).await.unwrap_err();
        assert_eq!(transport_code(&error).as_deref(), Some("EPROTO"), "{error:?}");
        assert_eq!(error.message, "EPROTO: The TLS handshake failed. Does the server speak TLS on this port?");
    }
    assert_eq!(server.seen().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unusable_keys_fail_like_bun() {
    let server = Server::start(true).await;
    let (cert, _) = client_cert("ocd-client");
    let (_, other_key) = client_cert("other");
    let base = format!("{}/v1", server.origin);
    for key in [other_key.as_str(), "garbage", cert.as_str()] {
        let error = chat(&provider(&base, Some(tls_value(&cert, key)), json!({}))).await.unwrap_err();
        assert_eq!(transport_code(&error).as_deref(), Some("FailedToOpenSocket"));
        assert_eq!(error.message, "FailedToOpenSocket: Was there a typo in the url or port?");
    }
    assert!(server.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_key_needs_its_passphrase() {
    use pkcs8_test::encrypt;
    let server = Server::start(true).await;
    let (cert, key) = client_cert("ocd-client");
    let encrypted = encrypt(&key, "secret");
    let base = format!("{}/v1", server.origin);
    let tls = json!({"cert": cert, "key": encrypted, "passphrase": "secret"});
    assert_eq!(chat(&provider(&base, Some(tls), json!({}))).await.unwrap(), "certified answer");
    let tls = json!({"cert": cert, "key": encrypted, "passphrase": "wrong"});
    let error = chat(&provider(&base, Some(tls), json!({}))).await.unwrap_err();
    assert_eq!(transport_code(&error).as_deref(), Some("FailedToOpenSocket"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_is_refused() {
    let server = Server::start(true).await;
    let (cert, key) = client_cert("ocd-client");
    let base = format!("{}/redirect/v1", server.origin);
    let error = chat(&provider(&base, Some(tls_value(&cert, &key)), json!({}))).await.unwrap_err();
    assert_eq!(transport_code(&error).as_deref(), Some("UnexpectedRedirect"));
    assert_eq!(
        error.message,
        format!("UnexpectedRedirect fetching \"{base}/chat/completions\". For more information, pass `verbose: true` in the second argument to fetch()")
    );
    // Only the redirect itself was requested.
    assert_eq!(server.seen().iter().map(|s| s.path.as_str()).collect::<Vec<_>>(), ["/redirect/v1/chat/completions"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_http_and_websocket_providers() {
    let server = Server::start(false).await;
    let (cert, key) = client_cert("ocd-client");
    // The certificate only goes to `https:` URLs: an unusable one doesn't fail a plain HTTP request.
    let base = format!("{}/v1", server.origin);
    assert_eq!(chat(&provider(&base, Some(tls_value(&cert, "garbage")), json!({}))).await.unwrap(), "certified answer");
    // A provider with `tls` stays on HTTP: the WebSocket transport can't present a certificate.
    let ws = provider(&base, Some(tls_value(&cert, &key)), json!({"transport": "websocket"}));
    let resolved = resolve_model(Some(&ws), &chat_model(), None, None, &Default::default()).unwrap();
    assert_eq!(resolved.transport.as_deref(), Some("http"));
    let ws = provider(&base, None, json!({"transport": "websocket"}));
    let resolved = resolve_model(Some(&ws), &chat_model(), None, None, &Default::default()).unwrap();
    assert_eq!(resolved.transport.as_deref(), Some("websocket"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_models_present_the_certificate() {
    let server = Server::start(true).await;
    let (cert, key) = client_cert("ocd-client");
    let corp = provider(&format!("{}/v1", server.origin), Some(tls_value(&cert, &key)), json!({}));
    let mut model = ModelInfo::default_for("corp", "decider");
    model.kind = Some("decision".into());
    model.package = Some("@opencode/ai/providers/openai-compatible/decisions".into());
    let MediaModel::Decision(decider) = resolve_media(Some(&corp), &model, MediaType::Decision, None, None, &Default::default()).unwrap() else {
        panic!("a decision model")
    };
    assert!(decider.http.as_ref().and_then(|h| h.tls.as_ref()).is_some());
    let questions = vec![json!({"name": "refund", "type": "predicate", "instructions": "Refund?"}).as_object().unwrap().clone()];
    let request = oc_ai::decision::DecisionRequest {
        model: decider,
        input: vec![oc_ai::decision::DecisionPart::Text("Please refund.".into())],
        questions,
        provider_options: None,
        http: None,
    };
    let response = oc_ai::decision::generate(&oc_util::http::client(), request).await.unwrap();
    assert_eq!(response.answers.len(), 1);
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].certified);
    assert_eq!(seen[0].path, "/v1/systemone");
}

/// PBES2 encryption of a PKCS#8 key at test time.
mod pkcs8_test {
    use base64::Engine;

    pub fn encrypt(pem: &str, passphrase: &str) -> String {
        use rustls::pki_types::pem::PemObject;
        let der = rustls::pki_types::PrivatePkcs8KeyDer::from_pem_slice(pem.as_bytes()).unwrap();
        let encrypted = oc_util::client_key::encrypt_pkcs8(der.secret_pkcs8_der(), passphrase);
        let text = base64::engine::general_purpose::STANDARD.encode(encrypted);
        let lines: Vec<&str> = text.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("-----BEGIN ENCRYPTED PRIVATE KEY-----\n{}\n-----END ENCRYPTED PRIVATE KEY-----\n", lines.join("\n"))
    }
}
