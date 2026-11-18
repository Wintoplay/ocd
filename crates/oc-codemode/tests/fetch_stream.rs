//! Streaming `fetch` responses (2.3.0, #63436) against a local HTTP server: every program of
//! `tests/fixtures/fetch-2.3.1.json` must give the `execute` output upstream core 2.3.1 gave against the same routes
//! (`tests/recorder/fetch-cases.ts` regenerates it), and the server must see the same bodies cancelled: a body still
//! open when the program ends, a cancelled reader, a `break` out of `for await`, and a redirect's body. Since 2.3.1
//! `fetch`'s arguments cross as JSON would carry them (#64513), and a program that only logs has no `null` line
//! (#64344).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::execute_tool::{output, ProgressRows};
use oc_codemode::host::{CallResult, ExtensionInvocation, Host, HostValue, Json, ToolCallError};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct Progress {
    rows: ProgressRows,
}

impl Host for Progress {
    fn call(&self, _path: &str, _input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        Box::pin(async { Ok(None) })
    }
    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>> {
        self.rows.extension_started(call);
        Box::pin(async { Ok(()) })
    }
    fn extension_after(&self, call: &ExtensionInvocation, result: &CallResult) {
        self.rows.settled(call.id, result);
    }
}

type Closed = Arc<Mutex<Vec<String>>>;

enum Body {
    Fixed(Vec<u8>),
    /// Chunked, 20 ms apart.
    Chunks(Vec<Vec<u8>>),
    /// One chunk, then open until the client goes away (recorded under this name).
    Open(Vec<u8>, &'static str),
    None,
}

fn route(path: &str) -> (u16, &'static str, Vec<(&'static str, &'static str)>, Body) {
    let text = "text/plain;charset=utf-8";
    let plain = |status, reason, body: &[u8]| (status, reason, vec![("content-type", text)], Body::Fixed(body.to_vec()));
    match path {
        "/json" => (200, "OK", vec![("content-type", "application/json;charset=utf-8")], Body::Fixed(br#"{"a":1,"b":[1,2]}"#.to_vec())),
        "/text" => plain(200, "OK", b"hello world"),
        "/chunks" => (200, "OK", vec![("content-type", text)], Body::Chunks(vec![b"ab".to_vec(), b"cd".to_vec(), b"ef".to_vec()])),
        "/split" => (200, "OK", vec![("content-type", text)], Body::Chunks(vec![vec![0xC3], vec![0xA9, 0xF0, 0x9F], vec![0x98, 0x80]])),
        "/open" => (200, "OK", vec![("content-type", text)], Body::Open(vec![42], "open")),
        "/empty" => plain(200, "OK", b""),
        "/nocontent" => (204, "No Content", vec![], Body::None),
        "/missing" => plain(404, "Not Found", b"no such thing"),
        "/redirect" => (302, "Found", vec![("content-type", text), ("location", "/json")], Body::Open(b"moved".to_vec(), "redirect")),
        "/headers" => (
            200,
            "OK",
            vec![("content-type", text), ("x-b", "2"), ("set-cookie", "a=1"), ("x-a", "1"), ("set-cookie", "b=2"), ("x-a", "3")],
            Body::Fixed(b"h".to_vec()),
        ),
        "/bom" => plain(200, "OK", &[0xEF, 0xBB, 0xBF, 0x68, 0x69, 0xFF]),
        "/bad-json" => plain(200, "OK", b"not json"),
        _ => plain(500, "Internal Server Error", b"unknown route"),
    }
}

/// Reads one request head; `None` when the connection closed.
async fn request(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<(String, String)> {
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            buffer.drain(..end + 4);
            let mut parts = head.split_whitespace();
            return Some((parts.next()?.to_string(), parts.next()?.to_string()));
        }
        let mut chunk = [0u8; 4096];
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

async fn chunk(socket: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    socket.write_all(format!("{:x}\r\n", bytes.len()).as_bytes()).await?;
    socket.write_all(bytes).await?;
    socket.write_all(b"\r\n").await?;
    socket.flush().await
}

async fn serve(mut socket: TcpStream, closed: Closed) {
    let mut buffer = Vec::new();
    while let Some((method, path)) = request(&mut socket, &mut buffer).await {
        if path == "/hang" {
            // Never answers; the request is recorded when it arrives and again when the client goes away.
            closed.lock().unwrap().push("hang requested".into());
            let mut rest = [0u8; 1024];
            while matches!(socket.read(&mut rest).await, Ok(n) if n > 0) {}
            closed.lock().unwrap().push("hang".into());
            return;
        }
        let (status, reason, headers, body) = route(&path);
        let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT");
        let mut head = format!("HTTP/1.1 {status} {reason}\r\ndate: {date}\r\n");
        for (name, value) in &headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        match &body {
            Body::Fixed(bytes) => head.push_str(&format!("content-length: {}\r\n", bytes.len())),
            Body::Chunks(_) | Body::Open(..) => head.push_str("transfer-encoding: chunked\r\n"),
            Body::None => {}
        }
        head.push_str("\r\n");
        if socket.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        if method == "HEAD" {
            continue;
        }
        match body {
            Body::Fixed(bytes) => {
                if socket.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            Body::Chunks(chunks) => {
                for bytes in chunks {
                    if chunk(&mut socket, &bytes).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if socket.write_all(b"0\r\n\r\n").await.is_err() {
                    return;
                }
            }
            Body::Open(first, name) => {
                let _ = chunk(&mut socket, &first).await;
                let mut rest = [0u8; 1024];
                // Held open until the client cancels the body by closing the connection.
                while matches!(socket.read(&mut rest).await, Ok(n) if n > 0) {}
                closed.lock().unwrap().push(name.to_string());
                return;
            }
            Body::None => {}
        }
    }
}

fn normalize(text: &str, base: &str) -> String {
    let date = regex::Regex::new(r"[A-Z][a-z]{2}, \d\d [A-Z][a-z]{2} \d{4} \d\d:\d\d:\d\d GMT").unwrap();
    date.replace_all(&text.replace(base, "http://BASE"), "DATE").into_owned()
}

async fn server() -> (String, Closed) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let closed: Closed = Arc::default();
    let accepted = closed.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket, accepted.clone()));
        }
    });
    (base, closed)
}

async fn until(closed: &Closed, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !closed.lock().unwrap().iter().any(|c| c == what) {
        assert!(tokio::time::Instant::now() < deadline, "the server never saw {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// tool-execute.test.ts "interrupting execute aborts a pending request": dropping the execution (how core interrupts
/// `execute`) closes the connection of a request still waiting for its response.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_execute_aborts_a_pending_request() {
    let (base, closed) = server().await;
    let client = Arc::new(oc_codemode::web::ReqwestClient::manual());
    let runtime = CodeMode::new(
        Vec::new(),
        Arc::new(Progress::default()),
        vec![oc_codemode::web::extension_checked(client, None, None)],
        ExecutionLimits::default(),
    )
    .unwrap();
    let code = format!("const res = await fetch({:?}); return res.status", format!("{base}/hang"));
    let running = tokio::spawn(async move { runtime.execute(&code).await });
    until(&closed, "hang requested").await;
    running.abort();
    until(&closed, "hang").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_fetch_matches_upstream() {
    let (base, closed) = server().await;
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fetch-2.3.1.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let code = case["code"].as_str().unwrap();
        let host = Arc::new(Progress::default());
        let client = Arc::new(oc_codemode::web::ReqwestClient::manual());
        let runtime = CodeMode::new(
            Vec::new(),
            host.clone(),
            vec![oc_codemode::web::extension_checked(client, None, None)],
            ExecutionLimits { timeout_ms: Some(10_000.0), ..Default::default() },
        )
        .unwrap();
        let result = runtime.execute(&format!("const BASE = {}; {code}", serde_json::to_string(&base).unwrap())).await;
        let ours = serde_json::to_string(&output(&result, host.rows.rows(), Vec::new())).unwrap();
        let ours: Value = serde_json::from_str(&normalize(&ours, &base)).unwrap();
        if ours != case["output"] {
            failures.push(format!("{code}\n  expected {}\n  got      {ours}", case["output"]));
        }
    }
    assert!(failures.is_empty(), "{} differ:\n\n{}", failures.len(), failures.join("\n\n"));
    // Cancelled bodies close their connection.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let expected: Vec<String> = fixture["closed"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
    loop {
        let mut seen = closed.lock().unwrap().clone();
        seen.sort();
        if seen == expected || tokio::time::Instant::now() > deadline {
            assert_eq!(seen, expected);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
