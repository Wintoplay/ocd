//! Cooperative stop (opencode 2.3.0 #62977): a user's stop lets each tool that listens for it (`Tool.Context.cancel`)
//! settle its own call and interrupts every other tool; a tool that does not settle within 5 s is interrupted too,
//! and any other interruption interrupts them all. The shell tool stops its command (SIGTERM, then SIGKILL after 3 s)
//! and fails the call with the output so far, the output file and the shell ID.
//!
//! Ports session-step.test.ts "stopTwoTools", session-run-coordinator.test.ts "tells why an execution is being stopped
//! while it cleans up" and tool-shell.test.ts "ShellTool stopped", against a scripted OpenAI-compatible mock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{body::Body, response::Response, routing::post, Router};
use oc_core::coordinator::Reason;
use oc_core::step::{self, Progress, StepError, ToolCall, ToolRun, ToolRunner};
use oc_core::{app::App, inbox::Scope, runner::Engine, session::{CreateInput, PromptInput}};
use oc_schema::message::{AssistantContent, ShellStatus, ToolState};
use oc_schema::types::{LocationRef, SessionError};
use oc_schema::Message;
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn sse(chunks: &[serde_json::Value]) -> Response {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    Response::builder().header("content-type", "text/event-stream").body(Body::from(body)).unwrap()
}

fn text(t: &str) -> Response {
    sse(&[
        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":t}}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ])
}

fn calls(calls: &[(&str, &str, serde_json::Value)]) -> Response {
    let mut chunks: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":index,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]})
        })
        .collect();
    chunks.push(json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}));
    sse(&chunks)
}

fn messages(app: &App, session: &str) -> Vec<Message> {
    app.db.session_messages(session).unwrap().into_iter().map(|(_, m)| m).collect()
}

fn tool_state(msgs: &[Message], call: &str) -> Option<ToolState> {
    msgs.iter().find_map(|m| match m {
        Message::Assistant(a) => a.content.iter().find_map(|c| match c {
            AssistantContent::Tool { id, state, .. } if id == call => Some(state.clone()),
            _ => None,
        }),
        _ => None,
    })
}

fn assistant_error(msgs: &[Message]) -> Option<SessionError> {
    msgs.iter().rev().find_map(|m| match m {
        Message::Assistant(a) => a.error.clone(),
        _ => None,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Stopped {
    /// Fails with its partial output.
    Fails,
    /// Fails with a decline (`Permission.DeclinedError`).
    Declines,
    /// Dies (`Effect.die`).
    Dies,
}

/// `stopTwoTools`: one tool stops on its stop, one never does.
struct TwoTools {
    stopped: Stopped,
    listening: Notify,
    stuck: Notify,
    interrupted: AtomicBool,
}

#[async_trait]
impl ToolRunner for TwoTools {
    async fn run(&self, call: ToolCall, _progress: Progress, cancel: CancellationToken, stop: oc_tools::Stop) -> ToolRun {
        if call.id == "call-cooperative" {
            // Like the shell tool: stops on its stop and fails with what it has.
            let listener = stop.listen().expect("a step call can be stopped");
            self.listening.notify_one();
            tokio::select! {
                _ = listener.stopped() => {}
                _ = cancel.cancelled() => return ToolRun::Interrupted,
            }
            return match self.stopped {
                Stopped::Fails => ToolRun::Failed {
                    error: SessionError::new("tool.execution", "partial output"),
                    metadata: Some(json!({"interrupted": true}).as_object().unwrap().clone()),
                },
                Stopped::Declines => ToolRun::Declined { question: None },
                Stopped::Dies => panic!("tool broke"),
            };
        }
        self.stuck.notify_one();
        cancel.cancelled().await;
        self.interrupted.store(true, Ordering::SeqCst);
        ToolRun::Interrupted
    }
}

struct NoStore;
#[async_trait]
impl oc_tools::websearch::WebSearchStore for NoStore {
    async fn kv_get(&self, _: &str) -> Option<serde_json::Value> {
        None
    }
    async fn kv_set(&self, _: &str, _: serde_json::Value) {}
    async fn kv_remove(&self, _: &str) {}
    async fn api_key(&self, _: &str) -> Option<String> {
        None
    }
}

fn config(port: u16, permissions: Option<serde_json::Value>) -> String {
    let mut config = json!({
        "model": "mock/m1",
        "providers": {"mock": {
            "name": "Mock",
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "test"},
            "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}}}
        }}
    });
    if let Some(p) = permissions {
        config["permissions"] = p;
    }
    config.to_string()
}

async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_stop_lets_tools_settle_their_own_calls() {
    let root = std::env::temp_dir().join(format!("oc-cooperative-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    let asking = root.join("asking");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&asking).unwrap();
    for (k, v) in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")] {
        unsafe { std::env::set_var(k, root.join(v)) };
    }
    unsafe {
        std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1");
        std::env::set_var("OPENCODE_TEST_HOME", root.join("home"));
    }

    // Each session runs one shell command (or two `test` calls); titles get a title.
    let commands: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    let k2 = commands.clone();
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: axum::http::HeaderMap, body: axum::Json<serde_json::Value>| {
            let title = body.0.to_string().contains("You name conversations");
            let session = headers.get("x-opencode-session-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            let command = if title { None } else { k2.lock().get(&session).cloned() };
            async move {
                match command.as_deref() {
                    None => text("Title"),
                    Some("two tools") => calls(&[("call-cooperative", "test", json!({})), ("call-stuck", "test", json!({}))]),
                    Some(command) => calls(&[("call_1", "shell", json!({"command": command}))]),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    std::fs::write(project.join("opencode.json"), config(port, None)).unwrap();
    std::fs::write(
        asking.join("opencode.json"),
        config(port, Some(json!([{"action": "shell", "resource": "*", "effect": "ask"}]))),
    )
    .unwrap();

    let global = oc_util::Global::acquire().unwrap();
    let db = oc_storage::Database::open(&global.database("latest"), &global.data).unwrap();
    let app = App::new(global, db, "2.0.26");
    let shells = oc_tools::shell::service::ShellService::new(app.global.data.clone(), None);
    let services = oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), reqwest::Client::new())),
        shell: shells.clone(),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(NoStore), reqwest::Client::new())),
        http: reqwest::Client::new(),
    };
    let engine = Engine::new(app.clone(), &services);
    engine.install();
    let session = |dir: &std::path::Path, command: &str| {
        let app = app.clone();
        let location = LocationRef { directory: dir.display().to_string(), workspace_id: None };
        let commands = commands.clone();
        let command = command.to_string();
        async move {
            let s = app.sessions.create(CreateInput { location: Some(location), title: Some("stop".into()), ..Default::default() }).await.unwrap();
            commands.lock().insert(s.id.clone(), command);
            s
        }
    };

    // ---- the step: one tool stops on its own, the other is interrupted ----
    let cases = [
        ("user", Stopped::Fails, false),
        ("user", Stopped::Fails, true),
        ("user", Stopped::Declines, false),
        ("user", Stopped::Dies, false),
        ("shutdown", Stopped::Fails, false),
    ];
    for (reason, stopped, location_closed) in cases {
        let s = session(&project, "two tools").await;
        let selected = engine.select(&s.id).await.unwrap();
        oc_core::instruction_state::prepare(&app.bus, &selected.sources, &s.id).unwrap();
        let loaded = engine.load(selected).await.unwrap();
        let (request, _) = engine.request(&loaded, false);
        let mut policy = oc_core::retry::Policy::default();
        let tools = Arc::new(TwoTools { stopped, listening: Notify::new(), stuck: Notify::new(), interrupted: AtomicBool::new(false) });
        let cancel = CancellationToken::new();
        let assistant = oc_schema::id::message();
        let why = if reason == "user" { Reason::User } else { Reason::Shutdown };
        let started = tokio::time::Instant::now();
        let attempt = step::attempt(step::Input {
            bus: app.bus.clone(),
            client: engine.client.clone(),
            session_id: s.id.clone(),
            assistant_message_id: assistant.clone(),
            agent: loaded.sel.agent.id.clone(),
            model: loaded.resolved.model_ref(),
            provider_metadata_key: "mock".into(),
            cost: vec![],
            request,
            tool_choice_none: false,
            tools: tools.clone(),
            snapshots: None,
            recover_continuation: true,
            retry: &mut policy,
            cancel: cancel.clone(),
            location_closed: Arc::new(move || location_closed),
            interruption: Some(Arc::new(move || Some(why))),
            retry_hook: None,
            http_hook: None,
            web_socket: None,
            recover_overflow: Box::new(|| Box::pin(async { false })),
        });
        let interrupt = async {
            tools.listening.notified().await;
            tools.stuck.notified().await;
            // The provider stream has completed, so the stop arrives while the step waits for its tools.
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        };
        let (outcome, ()) = tokio::join!(attempt, interrupt);
        let label = format!("{reason} {} closed={location_closed}", match stopped {
            Stopped::Fails => "fails",
            Stopped::Declines => "declines",
            Stopped::Dies => "dies",
        });
        assert!(started.elapsed() < Duration::from_secs(2), "{label}: {:?}", started.elapsed());
        assert!(tools.interrupted.load(Ordering::SeqCst), "{label}: the other tool is interrupted at once");
        let msgs = messages(&app, &s.id);
        let generic = |state: Option<ToolState>| match state {
            Some(ToolState::Error { error, .. }) => (error.kind, error.message),
            other => panic!("{label}: {other:?}"),
        };
        if stopped == Stopped::Dies {
            assert!(matches!(outcome, Err(StepError::Defect(ref e)) if e.to_string() == "tool broke"), "{label}");
            continue;
        }
        assert!(matches!(outcome, Err(StepError::Interrupted)), "{label}");
        let error = assistant_error(&msgs).expect("the step is recorded as interrupted");
        assert_eq!((error.kind.as_str(), error.message.as_str()), ("aborted", "Step interrupted"), "{label}");
        assert_eq!(
            generic(tool_state(&msgs, "call-stuck")),
            ("aborted".into(), "Tool execution interrupted".into()),
            "{label}"
        );
        match (reason, stopped) {
            ("user", Stopped::Fails) => match tool_state(&msgs, "call-cooperative") {
                Some(ToolState::Error { error, metadata, .. }) => {
                    assert_eq!((error.kind.as_str(), error.message.as_str()), ("tool.execution", "partial output"), "{label}");
                    assert_eq!(metadata, json!({"interrupted": true}).as_object().cloned(), "{label}");
                }
                other => panic!("{label}: {other:?}"),
            },
            ("user", Stopped::Declines) => assert_eq!(
                generic(tool_state(&msgs, "call-cooperative")),
                ("aborted".into(), "The user declined this tool call".into()),
                "{label}"
            ),
            _ => assert_eq!(
                generic(tool_state(&msgs, "call-cooperative")),
                ("aborted".into(), "Tool execution interrupted".into()),
                "{label}"
            ),
        }
    }

    // ---- the shell tool, through a whole run ----
    let run = |dir: std::path::PathBuf, command: &'static str| {
        let app = app.clone();
        let session = session.clone();
        async move {
            let s = session(&dir, command).await;
            app.sessions.prompt(&s.id, PromptInput { text: "run it".into(), ..Default::default() }).await.unwrap();
            app.coordinator().wake(&s.id, Scope::Input);
            s.id
        }
    };
    let running = |shells: &Arc<oc_tools::shell::service::ShellService>, with_output: bool| {
        shells.list().into_iter().find(|i| !with_output || shells.output(&i.id, None, None).map(|o| o.size > 0).unwrap_or(false))
    };
    let failure = |msgs: &[Message]| match tool_state(msgs, "call_1") {
        Some(ToolState::Error { error, metadata, .. }) => (error, metadata.unwrap_or_default()),
        other => panic!("{other:?}"),
    };

    // A stopped command keeps its file, and the call fails with the output so far, the file and the shell ID.
    let sid = run(project.clone(), "printf 'first\\nsecond\\n'; sleep 60").await;
    until("the command's output", || running(&shells, true).is_some()).await;
    let info = running(&shells, true).unwrap();
    assert!(app.coordinator().interrupt(&sid, Some(Reason::User), true).await);
    let msgs = messages(&app, &sid);
    let (error, metadata) = failure(&msgs);
    let notice = format!("[full output saved to {}]\n\nCommand interrupted by the user (shell ID: {}).", info.file, info.id);
    assert_eq!(error.kind, "tool.execution");
    assert_eq!(error.message, format!("first\nsecond\n\n{notice}"));
    assert_eq!(serde_json::Value::Object(metadata), json!({"interrupted": true, "truncated": false, "shellID": info.id, "notice": notice}));
    let ended = shells.wait(&info.id).await.unwrap();
    assert_eq!(ended.status, ShellStatus::Killed);
    assert_eq!(std::fs::read_to_string(&info.file).unwrap(), "first\nsecond\n");
    assert_eq!(assistant_error(&msgs).map(|e| e.message).as_deref(), Some("Step interrupted"));

    // A command that wrote nothing says so once.
    let sid = run(project.clone(), "sleep 60").await;
    until("the command", || running(&shells, false).is_some()).await;
    let info = running(&shells, false).unwrap();
    assert!(app.coordinator().interrupt(&sid, Some(Reason::User), true).await);
    let (error, _) = failure(&messages(&app, &sid));
    assert_eq!(error.message, format!("[no output; output file: {}]\n\nCommand interrupted by the user (shell ID: {}).", info.file, info.id));
    shells.wait(&info.id).await.unwrap();

    // A command that ignores SIGTERM: the call settles after the short wait, not the 3 s grace before SIGKILL.
    let sid = run(project.clone(), "trap '' TERM; printf 'kept\\n'; while :; do sleep 1; done").await;
    until("the command's output", || running(&shells, true).is_some()).await;
    let info = running(&shells, true).unwrap();
    let started = tokio::time::Instant::now();
    assert!(app.coordinator().interrupt(&sid, Some(Reason::User), true).await);
    assert!(started.elapsed() < Duration::from_millis(2500), "{:?}", started.elapsed());
    let (error, _) = failure(&messages(&app, &sid));
    assert_eq!(
        error.message,
        format!("kept\n\n[full output saved to {}]\n\nCommand interrupted by the user (shell ID: {}); it is still being stopped.", info.file, info.id)
    );
    // The stop still ends it.
    assert_eq!(tokio::time::timeout(Duration::from_secs(10), shells.wait(&info.id)).await.unwrap().unwrap().status, ShellStatus::Killed);

    // A server shutdown interrupts the call; the command is stopped and its file stays.
    let sid = run(project.clone(), "printf ready; sleep 60").await;
    until("the command's output", || running(&shells, true).is_some()).await;
    let info = running(&shells, true).unwrap();
    assert!(app.coordinator().interrupt(&sid, Some(Reason::Shutdown), true).await);
    let (error, metadata) = failure(&messages(&app, &sid));
    assert_eq!((error.kind.as_str(), error.message.as_str()), ("aborted", "Tool execution interrupted"));
    assert!(metadata.get("interrupted").is_none());
    assert_eq!(shells.wait(&info.id).await.unwrap().status, ShellStatus::Killed);
    assert_eq!(std::fs::read_to_string(&info.file).unwrap(), "ready");

    // Stopped while the call waits for permission: it ends without starting the command.
    let sid = run(asking.clone(), "printf started > started").await;
    until("the permission ask", || !app.permissions.list().is_empty()).await;
    assert!(app.coordinator().interrupt(&sid, Some(Reason::User), true).await);
    let (error, _) = failure(&messages(&app, &sid));
    assert_eq!(error.message, "Command interrupted by the user before it started.");
    assert!(app.permissions.list().is_empty());
    assert!(!asking.join("started").exists());
    assert!(shells.list().is_empty());

    std::fs::remove_dir_all(&root).ok();
}

// session-run-coordinator.test.ts: the reason is readable while the execution cleans up, and gone once it settled.
#[tokio::test]
async fn the_coordinator_tells_why_an_execution_is_being_stopped() {
    struct Driver {
        seen: Mutex<Vec<Option<Reason>>>,
        coordinator: std::sync::OnceLock<oc_core::coordinator::Coordinator>,
        started: Notify,
    }
    #[async_trait]
    impl oc_core::coordinator::Driver for Driver {
        async fn started(&self, _key: &str) {}
        async fn drain(&self, key: &str, _force: bool, _scope: Scope, cancel: CancellationToken) -> Result<(), oc_core::coordinator::DrainError> {
            let c = self.coordinator.get().unwrap();
            self.seen.lock().push(c.interruption(key));
            self.started.notify_one();
            cancel.cancelled().await;
            self.seen.lock().push(c.interruption(key));
            Err(oc_core::coordinator::DrainError::Interrupted)
        }
        async fn settled(&self, _key: &str, _exit: &oc_core::coordinator::Exit, _reason: Option<Reason>) {}
    }
    let driver = Arc::new(Driver { seen: Mutex::default(), coordinator: Default::default(), started: Notify::new() });
    let coordinator = oc_core::coordinator::Coordinator::new(driver.clone());
    let _ = driver.coordinator.set(coordinator.clone());
    let run = tokio::spawn({
        let c = coordinator.clone();
        async move { c.run("session").await }
    });
    driver.started.notified().await;
    assert!(coordinator.interrupt("session", Some(Reason::User), false).await);
    run.await.unwrap();
    assert_eq!(*driver.seen.lock(), vec![None, Some(Reason::User)]);
    assert_eq!(coordinator.interruption("session"), None);
}
