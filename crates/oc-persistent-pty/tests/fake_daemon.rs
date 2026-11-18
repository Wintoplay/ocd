//! Transport and service behaviour against the in-process protocol-9 fake (`oc_persistent_pty::testing`), ported
//! from upstream's `persistent-pty-daemon.test.ts` and `persistent-pty-session.test.ts`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use oc_persistent_pty::testing::{FakeDaemon, PID};
use oc_persistent_pty::{
    AttachInput, BinaryFn, CreatePipeInput, DaemonTransport, ErrorKind, Options, PersistentPty, PipeError, PipeLifecycle,
    Role, Signal, StreamEvent,
};
use serde_json::{Value, json};

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn no_spawn() -> BinaryFn {
    Arc::new(|| Box::pin(async { Err("spawning disabled in tests".to_string()) }))
}

fn binary(path: PathBuf) -> BinaryFn {
    Arc::new(move || {
        let path = path.to_string_lossy().into_owned();
        Box::pin(async move { Ok(path) })
    })
}

async fn service(root: &Path, id: &str) -> PersistentPty {
    let mut g = oc_util::Global::resolve();
    g.state = root.join("state");
    g.bin = root.join("bin");
    PersistentPty::new(
        &g,
        Options { root: Some(root.to_path_buf()), id: Some(id.into()), binary: Some(no_spawn()), ..Default::default() },
    )
    .await
}

async fn transport(root: &Path, id: &str) -> DaemonTransport {
    DaemonTransport::new(root.to_path_buf(), id.into(), no_spawn(), None).await
}

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("ocpty-fake-root-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

async fn poll(ready: impl Fn() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready(), "condition never held");
}

/// A fake with the three terminals the service tests use (ses_a: 1 and 3, ses_b: 2).
async fn with_terminals(directory: PathBuf) -> Arc<FakeDaemon> {
    let fake = FakeDaemon::start(directory).await;
    fake.add_terminal("ses_a");
    fake.add_terminal("ses_b");
    fake.add_terminal("ses_a");
    fake
}

#[tokio::test]
async fn list_without_daemon_is_empty_and_create_reports_spawn_failure() {
    let root = temp_root();
    let pty = service(&root, "absent").await;
    assert!(pty.list(None).await.unwrap().is_empty());
    let err = pty.create("ses", Default::default()).await.unwrap_err();
    assert_eq!(err, oc_persistent_pty::Error::Unavailable("spawning disabled in tests".into()));
    // A binary that cannot be spawned fails the same way again, so no fresh directory is tried.
    assert_eq!(pty.daemon().id(), "absent");
    // Without a daemon there are no pipes, and none is started for that.
    assert!(matches!(pty.list_pipes().await, Ok(list) if list.is_empty()));
    assert!(matches!(pty.get_pipe(1).await, Err(PipeError::NotFound(1))));
}

#[tokio::test]
async fn protocol_and_registration_mismatches() {
    let root = temp_root();
    let fake = FakeDaemon::start(root.join("d1")).await;
    fake.state.lock().protocol = 7;
    fake.write_registration();
    let pty = service(&root, "d1").await;
    let err = pty.daemon().discover(None).await.unwrap_err();
    assert_eq!(err.message, "opencode-pty protocol mismatch: daemon=7, client=9");
    assert_eq!(err.pid, Some(PID));
    // Protocol errors are not "connect" errors: list surfaces them.
    assert_eq!(pty.list(None).await.unwrap_err().to_string(), "opencode-pty protocol mismatch: daemon=7, client=9");

    let fake = FakeDaemon::start(root.join("d2")).await;
    fake.state.lock().pong_pid = Some(1);
    let pty = service(&root, "d2").await;
    assert_eq!(pty.daemon().discover(None).await.unwrap_err().message, "opencode-pty registration mismatch");

    // Protocol 9 registrations list their features.
    std::fs::create_dir_all(root.join("d3")).unwrap();
    std::fs::write(
        root.join("d3/service.json"),
        json!({"instance_id": "i", "pid": PID, "protocol": 9, "socket": "/none", "token": "t"}).to_string(),
    )
    .unwrap();
    let pty = service(&root, "d3").await;
    assert_eq!(pty.daemon().discover(None).await.unwrap_err().kind, ErrorKind::Protocol);
}

/// "rejects incompatible daemons without replacing or killing them"
#[tokio::test]
async fn rejects_incompatible_daemons_without_replacing_them() {
    let root = temp_root();
    let mut existing = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    std::fs::create_dir_all(root.join("daemon")).unwrap();
    std::fs::write(
        root.join("daemon/service.json"),
        json!({"instance_id": "old", "pid": existing.id(), "protocol": 7, "socket": "/unused", "token": "old", "features": []})
            .to_string(),
    )
    .unwrap();
    let daemon = DaemonTransport::new(root.clone(), "daemon".into(), binary("/missing/opencode-pty".into()), None).await;
    let optional = daemon.request_if_running(json!({"op": "list", "mode": "pty"})).await.unwrap_err();
    assert_eq!(
        (optional.kind, optional.message.as_str()),
        (ErrorKind::Protocol, "opencode-pty protocol mismatch: daemon=7, client=9")
    );
    assert_eq!(optional.pid, Some(existing.id() as u64));
    let starting = daemon.request(json!({"op": "create_pty"}), true).await.unwrap_err();
    assert_eq!(starting, optional);
    assert!(existing.try_wait().unwrap().is_none());
    existing.kill().unwrap();
    existing.wait().unwrap();
}

/// "gives up at once on a daemon binary that speaks another protocol, and never starts it again"
#[tokio::test]
async fn gives_up_at_once_on_a_binary_that_speaks_another_protocol() {
    let root = temp_root();
    let spawns = root.join("spawns");
    let script = root.join("old-opencode-pty");
    std::fs::write(
        &script,
        [
            "#!/bin/sh".to_string(),
            format!("echo spawn >> '{}'", spawns.display()),
            r#"mkdir -p "$3/$5""#.to_string(),
            r#"printf '{"instance_id":"old","pid":%s,"protocol":7,"socket":"none","token":"t","features":[]}' $$ > "$3/$5/service.json""#
                .to_string(),
            "exec sleep 30".to_string(),
        ]
        .join("\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let daemon = DaemonTransport::new(root.join("runtime"), "daemon".into(), binary(script), None).await;
    let started = std::time::Instant::now();
    let first = daemon.request(json!({"op": "list", "mode": "pipe"}), true).await.unwrap_err();
    assert_eq!(first.kind, ErrorKind::Protocol);
    let second = daemon.request(json!({"op": "list", "mode": "pipe"}), true).await.unwrap_err();
    assert_eq!(second.kind, ErrorKind::Protocol);
    // Well under the 5 s spawn timeout: the daemon was not waited on again.
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(std::fs::read_to_string(&spawns).unwrap().trim().lines().count(), 1);
}

#[tokio::test]
async fn owns_lists_attaches_and_hands_off() {
    let root = temp_root();
    let fake = with_terminals(root.join("d")).await;
    let pty = service(&root, "d").await;

    let all = pty.list(None).await.unwrap();
    assert_eq!(
        all.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        vec!["pty_persistent_1", "pty_persistent_2", "pty_persistent_3"]
    );
    assert_eq!(pty.list(Some("ses_b")).await.unwrap().len(), 1);
    assert!(pty.daemon().is_owner());
    {
        let requests = fake.requests();
        assert_eq!(requests[0], json!({"op": "ping"}));
        assert_eq!(requests[1], json!({"op": "own", "instance_id": "inst-1"}));
        assert_eq!(requests[2], json!({"op": "list", "mode": "pty"}));
    }
    // `get` asks for the one process; IDs that are not ours name no terminal.
    assert_eq!(pty.get("pty_persistent_2").await.unwrap().session_id, "ses_b");
    assert!(fake.requests().contains(&json!({"op": "get", "id": 2})));
    for id in ["pty_persistent_9", "pty_1", "pty_persistent_0", "pty_persistent_x"] {
        assert_eq!(pty.get(id).await.unwrap_err(), oc_persistent_pty::Error::NotFound(id.into()));
    }

    let mut attachment = pty
        .attach("pty_persistent_1", AttachInput { cursor: 3, attachment_id: "a1".into(), role: Role::Controller, takeover: true })
        .await
        .unwrap();
    assert_eq!(attachment.replay.data, b"hello");
    assert_eq!(attachment.replay.requested_offset, 3);
    assert_eq!(attachment.replay.end_offset, 5);
    assert_eq!(attachment.info.session_id, "ses_a");
    let subscribe = fake.ops("subscribe").pop().unwrap();
    assert_eq!(
        subscribe,
        json!({"op": "subscribe", "id": 1, "offset": 3, "attachment_id": "a1", "role": "controller", "takeover": true})
    );
    let mut stream = attachment.activate().unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.recv().await {
        events.push(event);
    }
    assert_eq!(
        events,
        vec![
            StreamEvent::Output { start: 5, end: 8, data: b"abc".to_vec() },
            StreamEvent::TitleChanged { title: "vim".into() },
            StreamEvent::Exited { exit_code: Some(0), final_offset: 8 },
        ]
    );
    // The observed exit triggers a background terminate.
    poll(|| fake.requests().contains(&json!({"op": "terminate", "id": 1}))).await;

    // Controller attach selected the terminal; resize with attachment null.
    pty.resize("pty_persistent_2", 120, 40, None).await.unwrap();
    assert!(fake.requests().contains(&json!({"op": "resize", "id": 2, "attachment_id": null, "cols": 120, "rows": 40})));

    assert!(!pty.handoff_prepared());
    let handoff = pty.handoff().await.unwrap().unwrap();
    assert_eq!(handoff.directory, root.join("d").to_string_lossy());
    assert_eq!((handoff.instance_id.as_str(), handoff.ticket.as_str()), ("inst-1", "tick"));
    assert!(handoff.expires_at > now_ms());
    assert!(fake.requests().contains(&json!({"op": "prepare_handoff"})));
    assert!(pty.handoff_prepared());

    // Losing the owner connection drops the registration; the next request re-discovers and re-claims.
    fake.owner_close.notify_waiters();
    poll(|| !pty.daemon().is_owner()).await;
    assert_eq!(pty.list(None).await.unwrap().len(), 2);
    assert!(pty.daemon().is_owner());
    assert_eq!(fake.ops("own").len(), 2);

    // A shutdown clears the prepared handoff.
    pty.shutdown().await.unwrap();
    assert!(!pty.handoff_prepared());
}

/// A prepared handoff counts only while its ticket is valid.
#[tokio::test]
async fn an_expired_handoff_is_not_prepared() {
    let root = temp_root();
    let fake = FakeDaemon::start(root.join("d")).await;
    fake.state.lock().handoff_ttl = 50;
    let pty = service(&root, "d").await;
    pty.list(None).await.unwrap();
    pty.handoff().await.unwrap().unwrap();
    assert!(pty.handoff_prepared());
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!pty.handoff_prepared());
}

#[tokio::test]
async fn authentication_failure_rediscovers_once() {
    let root = temp_root();
    let fake = with_terminals(root.join("d")).await;
    let pty = service(&root, "d").await;
    assert_eq!(pty.list(None).await.unwrap().len(), 3);
    // Next list: auth fails once (token rotated on disk), retried with the new registration.
    fake.auth_failures.store(1, Ordering::SeqCst);
    assert_eq!(pty.list(None).await.unwrap().len(), 3);
    // Two consecutive failures surface the error.
    fake.auth_failures.store(2, Ordering::SeqCst);
    assert_eq!(pty.list(None).await.unwrap_err().to_string(), "authentication failed");
}

/// "rediscovers a same-protocol daemon after its registration rotates" and "… when acquiring a subscription"
#[tokio::test]
async fn rediscovers_a_rotated_registration() {
    let root = temp_root();
    let fake = with_terminals(root.join("daemon")).await;
    let daemon = transport(&root, "daemon").await;
    daemon.request(json!({"op": "list", "mode": "pty"}), false).await.unwrap();
    for (token, instance) in [("new-token", "new-instance"), ("newest-token", "newest-instance")] {
        {
            let mut state = fake.state.lock();
            state.token = token.into();
            state.instance = instance.into();
        }
        fake.write_registration();
        let running = daemon.request_if_running(json!({"op": "list", "mode": "pty"})).await.unwrap();
        assert!(matches!(running, Some(oc_persistent_pty::wire::WireResponse::Processes { .. })));
    }
    fake.state.lock().token = "subscribe-token".into();
    fake.write_registration();
    let input =
        oc_persistent_pty::SubscribeInput { cursor: 0, attachment_id: "attachment".into(), role: Role::Observer, takeover: false };
    let attachment = daemon.subscribe(1, &input).await.unwrap();
    assert_eq!(attachment.terminal.id, 1);
    assert_eq!(fake.ops("subscribe").len(), 1);
    attachment.detach();
}

/// "retries a start-required request when connection fails before dispatch"
#[tokio::test]
async fn retries_a_start_required_request_when_the_connection_fails_before_dispatch() {
    let root = temp_root();
    let first = FakeDaemon::start(root.join("daemon")).await;
    let daemon = transport(&root, "daemon").await;
    daemon.request(json!({"op": "list", "mode": "pty"}), false).await.unwrap();
    first.close();
    let second = FakeDaemon::start(root.join("daemon")).await;
    daemon.request(json!({"op": "create_pty", "group_id": "ses"}), true).await.unwrap();
    assert_eq!(second.ops("create_pty").len(), 1);
    assert!(first.ops("create_pty").is_empty());
}

/// "does not replay a dispatched mutating request when its response is lost"
#[tokio::test]
async fn does_not_replay_a_dispatched_request_whose_response_was_lost() {
    let root = temp_root();
    let fake = FakeDaemon::start(root.join("daemon")).await;
    fake.state.lock().hang_up.insert("create_pty".into());
    let daemon = transport(&root, "daemon").await;
    let error = daemon.request(json!({"op": "create_pty"}), true).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Response);
    assert_eq!(fake.ops("create_pty").len(), 1);
}

/// "waits on its own connection until the process exits", "closes the wait connection when the wait is
/// interrupted", "reports a daemon that goes away during a wait as unreachable"
#[tokio::test]
async fn waits_on_its_own_connection() {
    let root = temp_root();
    let fake = FakeDaemon::start(root.join("daemon")).await;
    let pty = service(&root, "daemon").await;
    let file = root.join("out");
    let input = |command: &str| CreatePipeInput {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), command.into()],
        cwd: root.to_string_lossy().into_owned(),
        env: [("A".to_string(), "1".to_string())].into(),
        group_id: "proj".into(),
        output_path: file.to_string_lossy().into_owned(),
        metadata: json!({"id": "sh_1"}).as_object().cloned().unwrap(),
    };
    let created = pty.create_pipe(input("echo hi")).await.unwrap();
    assert_eq!((created.id, created.pid, created.lifecycle.clone()), (1, Some(1001), PipeLifecycle::Running));
    assert_eq!(created.command, vec!["/bin/sh", "-c", "echo hi"]);
    let request = fake.ops("create_pipe").pop().unwrap();
    assert_eq!(
        request,
        json!({"op": "create_pipe", "program": "/bin/sh", "args": ["-c", "echo hi"], "cwd": root.to_string_lossy(),
               "env": {"A": "1"}, "group_id": "proj", "output_path": file.to_string_lossy(), "metadata": {"id": "sh_1"}})
    );

    // An unknown process is the daemon's own error.
    assert!(matches!(pty.wait_pipe(2).await, Err(PipeError::Request(m)) if m == "terminal 2 not found"));
    let error = pty.daemon().wait(2).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Request);

    let waiting = tokio::spawn({
        let pty = pty.clone();
        async move { pty.wait_pipe(1).await }
    });
    poll(|| fake.waiters(1) == 1).await;
    fake.output(1, "hi\n");
    fake.exit(1, None, Some("SIGTERM"));
    let exit = waiting.await.unwrap().unwrap();
    assert_eq!(
        (exit.exit_code, exit.signal.as_deref(), exit.final_offset, exit.output_lines),
        (None, Some("SIGTERM"), 3, 1)
    );
    // An exited process is answered at once.
    assert_eq!(pty.wait_pipe(1).await.unwrap().final_offset, 3);

    // Interrupting a wait closes its connection.
    pty.create_pipe(input("sleep 1")).await.unwrap();
    let waiting = tokio::spawn({
        let pty = pty.clone();
        async move { pty.wait_pipe(2).await }
    });
    poll(|| fake.waiters(2) == 1).await;
    waiting.abort();
    poll(|| fake.waiters(2) == 0).await;

    // A daemon that goes away during a wait is unreachable.
    let waiting = tokio::spawn({
        let pty = pty.clone();
        async move { pty.daemon().wait(2).await }
    });
    poll(|| fake.waiters(2) == 1).await;
    fake.drop_waiters(2);
    assert_eq!(waiting.await.unwrap().unwrap_err().kind, ErrorKind::Connect);
}

/// The pipe methods: get, list, signal, set_metadata, terminate, and their errors.
#[tokio::test]
async fn pipe_processes() {
    let root = temp_root();
    let fake = FakeDaemon::start(root.join("daemon")).await;
    fake.add_terminal("ses_a");
    let pty = service(&root, "daemon").await;
    let out = root.join("pipe.out");
    let input = CreatePipeInput {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "x".into()],
        cwd: root.to_string_lossy().into_owned(),
        env: Default::default(),
        group_id: "proj".into(),
        output_path: out.to_string_lossy().into_owned(),
        metadata: Default::default(),
    };
    let created = pty.create_pipe(input.clone()).await.unwrap();
    assert_eq!(created.id, 2);
    // A terminal is no pipe, and a pipe no terminal.
    assert!(matches!(pty.get_pipe(1).await, Err(PipeError::NotFound(1))));
    assert_eq!(
        pty.get("pty_persistent_2").await.unwrap_err(),
        oc_persistent_pty::Error::NotFound("pty_persistent_2".into())
    );
    assert_eq!(pty.list(None).await.unwrap().len(), 1);
    assert_eq!(pty.list_pipes().await.unwrap().iter().map(|p| p.id).collect::<Vec<_>>(), vec![2]);

    fake.output(2, "a\nb");
    let found = pty.get_pipe(2).await.unwrap();
    assert_eq!((found.output_tail, found.output_lines, found.output_path.as_str()), (3, 1, out.to_str().unwrap()));
    pty.set_pipe_metadata(2, json!({"stopReason": "killed"}).as_object().cloned().unwrap()).await.unwrap();
    assert_eq!(pty.get_pipe(2).await.unwrap().metadata["stopReason"], "killed");

    fake.state.lock().trap.insert(2);
    pty.signal_pipe(2, Signal::Term).await.unwrap();
    assert_eq!(pty.get_pipe(2).await.unwrap().lifecycle, PipeLifecycle::Running);
    pty.signal_pipe(2, Signal::Kill).await.unwrap();
    assert_eq!(fake.signals(2), vec!["SIGTERM", "SIGKILL"]);
    let ended = pty.get_pipe(2).await.unwrap();
    assert_eq!(ended.lifecycle, PipeLifecycle::Exited { exit_code: None, signal: Some("SIGKILL".into()) });
    assert!(ended.exited_at.is_some());

    pty.terminate_pipe(2).await.unwrap();
    assert!(matches!(pty.get_pipe(2).await, Err(PipeError::NotFound(2))));
    assert!(matches!(pty.signal_pipe(2, Signal::Term).await, Err(PipeError::Request(_))));

    // A rejected request may have run: it is no "unavailable".
    fake.state.lock().fail_create = Some("spawn failed".into());
    assert_eq!(pty.create_pipe(input.clone()).await.unwrap_err(), PipeError::Request("spawn failed".into()));
    let mut relative = input;
    relative.cwd = "relative".into();
    fake.state.lock().fail_create = None;
    assert_eq!(pty.create_pipe(relative).await.unwrap_err(), PipeError::Request("cwd must be absolute".into()));
}

#[tokio::test]
async fn expired_or_mismatched_handoff_is_discarded() {
    let root = temp_root();
    let fake = FakeDaemon::start(root.join("old")).await;
    let mut g = oc_util::Global::resolve();
    g.state = root.join("state");
    let mut handoff = fake.handoff();
    handoff.instance_id = "other-instance".into();
    let pty = PersistentPty::new(
        &g,
        Options { root: Some(root.clone()), handoff: Some(handoff), binary: Some(no_spawn()), ..Default::default() },
    )
    .await;
    assert!(!pty.daemon().is_owner());
    assert_ne!(pty.daemon().id(), "old");
    // The mismatched daemon was asked to shut down.
    assert_eq!(fake.ops("shutdown").len(), 1);
}

/// "starts fresh instead of failing when an inherited registration is gone or expired"
#[tokio::test]
async fn starts_fresh_when_an_inherited_registration_is_gone_or_expired() {
    let root = temp_root();
    let legacy = root.join("legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    for expires_at in [now_ms() + 60_000, now_ms() - 1] {
        let handoff = oc_persistent_pty::Handoff {
            directory: legacy.to_string_lossy().into_owned(),
            instance_id: "test".into(),
            ticket: "ticket".into(),
            expires_at,
        };
        let daemon = DaemonTransport::new(root.join("root"), "fresh".into(), no_spawn(), Some(handoff)).await;
        assert_eq!(daemon.handoff().await.unwrap(), None);
        assert_eq!(daemon.request_if_running(json!({"op": "list", "mode": "pty"})).await.unwrap(), None);
    }
}

/// A valid handoff adopts the daemon in its own directory and claims it with the ticket.
#[tokio::test]
async fn adopts_a_handed_off_daemon() {
    let root = temp_root();
    let fake = FakeDaemon::start(root.join("inherited")).await;
    let mut g = oc_util::Global::resolve();
    g.state = root.join("state");
    let pty = PersistentPty::new(
        &g,
        Options {
            root: Some(root.join("other-root")),
            handoff: Some(fake.handoff()),
            binary: Some(no_spawn()),
            ..Default::default()
        },
    )
    .await;
    assert!(pty.daemon().is_owner());
    assert_eq!(pty.daemon().id(), "inherited");
    assert!(fake.requests().contains(&json!({"op": "own", "instance_id": "inst-1", "ticket": "ticket"})));
    // Nothing was moved: the daemon keeps its directory.
    assert!(root.join("inherited/service.json").exists());
}

/// persistent-pty-daemon.test.ts (#60329): `rename` sends `{op:"rename", id, title}`, returns the terminal with its
/// `userTitle` and publishes `persistent-pty.updated`. Protocol 9 daemons always support it.
#[tokio::test]
async fn rename_publishes_updated() {
    let root = temp_root();
    let fake = with_terminals(root.join("new")).await;
    let pty = service(&root, "new").await;
    // A missing terminal is reported first.
    assert_eq!(
        pty.rename("pty_persistent_9", None).await.unwrap_err(),
        oc_persistent_pty::Error::NotFound("pty_persistent_9".into())
    );
    let mut events = pty.subscribe_events();
    let renamed = pty.rename("pty_persistent_1", Some("Build")).await.unwrap();
    assert_eq!(renamed.user_title.as_deref(), Some("Build"));
    assert!(fake.requests().contains(&json!({"op": "rename", "id": 1, "title": "Build"})));
    let event = events.recv().await.unwrap();
    assert_eq!(event.event_type(), "persistent-pty.updated");
    assert_eq!(event.properties()["sessionID"], "ses_a");
    assert_eq!(event.properties()["terminal"]["userTitle"], "Build");
    let cleared = pty.rename("pty_persistent_1", None).await.unwrap();
    assert_eq!(cleared.user_title, None);
    assert!(fake.requests().contains(&json!({"op": "rename", "id": 1, "title": null})));
    assert_eq!(serde_json::to_value(&cleared).unwrap()["userTitle"], Value::Null);
}

/// persistent-pty-session.test.ts (#60322): deleting a session removes its terminals, publishing
/// `persistent-pty.removed` for each, and leaves other sessions' terminals alone.
#[tokio::test]
async fn remove_session_terminates_the_sessions_terminals() {
    let root = temp_root();
    let fake = with_terminals(root.join("d")).await;
    let pty = service(&root, "d").await;
    let mut events = pty.subscribe_events();
    pty.remove_session("ses_a").await;
    assert_eq!(fake.ops("terminate"), vec![json!({"op": "terminate", "id": 1}), json!({"op": "terminate", "id": 3})]);
    for id in ["pty_persistent_1", "pty_persistent_3"] {
        let event = events.recv().await.unwrap();
        assert_eq!(event.event_type(), "persistent-pty.removed");
        assert_eq!(event.properties(), json!({"sessionID": "ses_a", "ptyID": id}));
    }
    // Without a running daemon there is nothing to remove, and nothing fails.
    let absent = service(&root, "absent").await;
    absent.remove_session("ses_a").await;
}
