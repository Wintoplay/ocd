//! `shell-daemon.test.ts` (2.3.1): the shell service on the process daemon, against the in-process fake
//! (`oc_persistent_pty::testing`). Each `server()` is a server of its own over the same daemon, as a restart is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oc_persistent_pty::testing::FakeDaemon;
use oc_persistent_pty::{BinaryFn, Options, PersistentPty};
use oc_schema::Metadata;
use oc_schema::message::{ShellInfo, ShellStatus};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::daemon::{LOST, Placement, ToolCall};
use super::service::{ShellEvent, ShellService};
use crate::host::ShellInvocation;

struct Env {
    _dir: tempfile::TempDir,
    data: PathBuf,
    directory: String,
    fake: Arc<FakeDaemon>,
    pty: PersistentPty,
    events: Arc<Mutex<Vec<ShellEvent>>>,
}

fn no_spawn() -> BinaryFn {
    Arc::new(|| Box::pin(async { Err("spawning disabled in tests".to_string()) }))
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let data = root.join("data");
    let directory = root.join("project");
    std::fs::create_dir_all(&directory).unwrap();
    let fake = FakeDaemon::start(root.join("root/inherited")).await;
    let mut global = oc_util::Global::resolve();
    global.state = root.join("state");
    global.bin = root.join("bin");
    let pty = PersistentPty::new(
        &global,
        Options { root: Some(root.join("root")), handoff: Some(fake.handoff()), binary: Some(no_spawn()), ..Default::default() },
    )
    .await;
    Env { _dir: dir, data, directory: directory.to_string_lossy().into_owned(), fake, pty, events: Arc::default() }
}

impl Env {
    /// A server of its own over the shared daemon (a restart builds a new one).
    fn server(&self) -> Arc<ShellService> {
        let events = self.events.clone();
        let service = ShellService::new(self.data.clone(), Some(Arc::new(move |event| events.lock().push(event))));
        service.attach_daemon(self.pty.clone());
        service
    }

    fn placement(&self, directory: &str) -> Placement {
        Placement { project_id: "proj".into(), directory: directory.into(), ..Default::default() }
    }

    fn created(&self) -> Vec<Value> {
        self.fake.ops("create_pipe")
    }

    fn metadata(&self, id: u64) -> Value {
        self.fake.process(id).map(|p| p["pipe"]["metadata"].clone()).unwrap_or(Value::Null)
    }
}

fn invocation(command: &str, timeout: u64) -> ShellInvocation {
    ShellInvocation { command: command.into(), cwd: String::new(), timeout, shell: "/bin/sh".into(), env: HashMap::new() }
}

async fn create(service: &Arc<ShellService>, env: &Env, command: &str, timeout: u64) -> ShellInfo {
    service.create(&invocation(command, timeout), Metadata::new(), &env.placement(&env.directory)).await.unwrap()
}

async fn until(check: impl Fn() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition never held");
}

/// Fails instead of hanging when `future` does not end within 10 s.
async fn within<T>(label: &str, future: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(Duration::from_secs(10), future).await {
        Ok(value) => value,
        Err(_) => panic!("{label} did not end"),
    }
}

fn lines(count: usize) -> String {
    (1..=count).map(|i| format!("line {i}\n")).collect()
}

#[tokio::test]
async fn runs_a_command_with_the_servers_environment_an_absolute_cwd_and_its_metadata() {
    let env = env().await;
    std::fs::create_dir_all(Path::new(&env.directory).join("sub")).unwrap();
    let service = env.server();
    let mut run = invocation("echo hi", 60_000);
    run.cwd = "sub".into();
    run.env = [("TERM".to_string(), "xterm-256color".to_string()), ("OPENCODE_TERMINAL".to_string(), "1".to_string())].into();
    let metadata: Metadata = json!({"sessionID": "ses_test"}).as_object().cloned().unwrap();
    let info = service.create(&run, metadata, &env.placement(&env.directory)).await.unwrap();
    let request = env.created().pop().unwrap();
    let cwd = Path::new(&env.directory).join("sub").to_string_lossy().into_owned();
    assert_eq!(request["program"], "/bin/sh");
    assert_eq!(request["args"], json!(["-c", "echo hi"]));
    assert_eq!(request["cwd"], cwd);
    assert_eq!(request["output_path"], info.file);
    assert_eq!(request["group_id"], "proj");
    assert_eq!(request["env"], json!({"TERM": "xterm-256color", "OPENCODE_TERMINAL": "1"}));
    let stored = &request["metadata"];
    assert_eq!(
        (stored["id"].as_str(), stored["command"].as_str(), stored["shell"].as_str()),
        (Some(info.id.as_str()), Some("echo hi"), Some("/bin/sh"))
    );
    assert_eq!(stored["location"], json!({"directory": env.directory}));
    assert_eq!(stored["metadata"], json!({"sessionID": "ses_test"}));
    assert!(stored["timeoutDeadline"].is_u64(), "{stored}");
    assert!(stored.get("toolCall").is_none() && stored.get("stopReason").is_none());
    assert_eq!((info.status, info.pid, info.cwd.as_str()), (ShellStatus::Running, Some(1001), cwd.as_str()));
    assert!(info.file.starts_with(&env.data.join("shell/proj").to_string_lossy().into_owned()));
    // `shell.created` names the location and carries the daemon's pid.
    assert!(matches!(&env.events.lock()[0], ShellEvent::Created { info: created, location: Some(l) } if created.pid == Some(1001) && l.directory == env.directory));

    env.fake.output(1, "first\nsecond");
    let page = service.output(&info.id, None, None).unwrap();
    assert_eq!((page.output.as_str(), page.cursor, page.size, page.truncated), ("first\nsecond", 12, 12, false));
    let page = service.output(&info.id, Some(6), Some(3)).unwrap();
    assert_eq!((page.output.as_str(), page.cursor), ("sec", 9));
    assert_eq!(service.output(&info.id, Some(u64::MAX >> 11), None).unwrap().cursor, 12);
    assert_eq!(service.list().iter().map(|i| i.id.clone()).collect::<Vec<_>>(), vec![info.id.clone()]);

    env.fake.exit(1, Some(0), None);
    let result = service.result(&info, 2000, 51200).await;
    assert_eq!((result.info.status, result.info.exit), (ShellStatus::Exited, Some(0)));
    assert!(result.info.time.completed.is_some());
    assert_eq!(result.capture.unwrap().output, "first\nsecond");
    assert!(service.list().is_empty());
}

#[tokio::test]
async fn pages_shell_output_at_utf8_boundaries_with_byte_cursors() {
    let env = env().await;
    let service = env.server();
    let info = create(&service, &env, "pages", 0).await;
    let prefix = "a".repeat(65535);
    for c in ["é", "€", "😀"] {
        let text = format!("{prefix}{c}\u{FEFF}z");
        std::fs::write(&info.file, &text).unwrap();
        let page = service.output(&info.id, None, None).unwrap();
        assert_eq!((page.cursor, page.size), (prefix.len() as u64, text.len() as u64));
        let rest = service.output(&info.id, Some(page.cursor), None).unwrap();
        assert_eq!(format!("{}{}", page.output, rest.output), text);
        let tiny = service.output(&info.id, Some(page.cursor), Some(1)).unwrap();
        assert_eq!((tiny.output.as_str(), tiny.cursor), (c, page.cursor + c.len() as u64));
        assert_eq!(service.output(&info.id, Some(page.cursor + 1), None).unwrap().output, "\u{FEFF}z");
    }
}

#[tokio::test]
async fn counts_lines_like_the_spawner_and_reads_only_what_reached_the_file_after_a_write_error() {
    let env = env().await;
    let service = env.server();
    let long = create(&service, &env, "long", 0).await;
    env.fake.output(1, &lines(3000));
    env.fake.exit(1, Some(0), None);
    let capture = service.result(&long, 2000, 51200).await.capture.unwrap();
    assert!(capture.output.ends_with(&format!("line 3000\n\n[showing lines 1001-3000 of 3000; full output saved to {}]", long.file)));

    let failed = create(&service, &env, "failed", 0).await;
    env.fake.output(2, "kept\n");
    env.fake.fail(2, "No space left on device");
    env.fake.output(2, "lost\n");
    env.fake.exit(2, Some(1), None);
    let result = service.result(&failed, 2000, 51200).await;
    assert_eq!((result.info.status, result.info.exit), (ShellStatus::Exited, Some(1)));
    assert_eq!(
        result.capture.unwrap().output,
        format!("kept\n\n[output file {} stopped early: No space left on device]", failed.file)
    );

    // A single line longer than the window is sized from the file, which the daemon does not track.
    let wide = create(&service, &env, "wide", 0).await;
    env.fake.output(3, &format!("short\n{}\n", "x".repeat(60 * 1024)));
    env.fake.exit(3, Some(0), None);
    assert!(service.result(&wide, 2000, 51200).await.capture.unwrap().output.ends_with(&format!(
        "\n\n[showing the last 50 KB of line 2 (60 KB); full output saved to {}]",
        wide.file
    )));
}

#[tokio::test]
async fn bounds_what_it_reads_to_number_lines_the_daemon_has_not_counted_yet() {
    let env = env().await;
    let service = env.server();
    let now = Some(Duration::ZERO);
    // Still running, so the daemon has not reported a line count, and the file is too big to count quickly.
    let big = create(&service, &env, "big", 0).await;
    env.fake.output(1, &format!("{}\n{}", "y".repeat(17 * 1024 * 1024), lines(10)));
    let output = service.result_within(&big, now, 2000, 51200).await.capture.unwrap().output;
    assert!(output.ends_with(&format!("line 10\n\n[showing the last 10 lines; full output saved to {}]", big.file)));

    // A cut-off line that started too far back to look for, with or without a final newline.
    for (id, end) in [(2, ""), (3, "\n")] {
        let wide = create(&service, &env, "wide", 0).await;
        env.fake.output(id, &format!("short\n{}{end}", "x".repeat(2 * 1024 * 1024)));
        let output = service.result_within(&wide, now, 2000, 51200).await.capture.unwrap().output;
        assert!(
            output.ends_with(&format!("\n\n[showing the last 50 KB of line 2 (over 1024 KB); full output saved to {}]", wide.file)),
            "{}",
            &output[output.len() - 200..]
        );
    }
}

#[tokio::test]
async fn reports_a_command_lost_with_the_daemon_as_killed_with_what_it_wrote() {
    let env = env().await;
    let service = env.server();
    let info = create(&service, &env, "work", 0).await;
    env.fake.output(1, "partial\n");
    until(|| env.fake.waiters(1) > 0).await;
    env.fake.drop_waiters(1);
    let result = service.result(&info, 2000, 51200).await;
    assert_eq!(result.info.status, ShellStatus::Killed);
    let capture = result.capture.unwrap();
    assert_eq!((capture.output.as_str(), capture.killed.as_deref()), ("partial\n", Some(LOST)));
    // The notice says why.
    assert_eq!(super::result::notice(&super::result::output(&ShellResult::of(&info, capture)), None).as_deref(), Some(LOST));
}

struct ShellResult;
impl ShellResult {
    fn of(info: &ShellInfo, capture: super::result::Capture) -> super::result::ShellResult {
        super::result::ShellResult { info: info.clone(), capture: Some(capture) }
    }
}

#[tokio::test]
async fn records_successful_and_failed_exits_without_signalling_a_finished_command() {
    let env = env().await;
    let service = env.server();
    let succeeded = create(&service, &env, "serve & exit 0", 0).await;
    let failed = create(&service, &env, "serve & exit 3", 0).await;
    until(|| env.fake.waiters(1) > 0 && env.fake.waiters(2) > 0).await;
    env.fake.exit(1, Some(0), None);
    env.fake.exit(2, Some(3), None);
    let ended = service.wait(&failed.id).await.unwrap();
    assert_eq!((ended.status, ended.exit), (ShellStatus::Exited, Some(3)));
    assert_eq!(service.wait(&succeeded.id).await.unwrap().exit, Some(0));
    assert!(env.fake.signals(1).is_empty() && env.fake.signals(2).is_empty());
}

#[tokio::test]
async fn fails_a_command_the_daemon_rejected_instead_of_spawning_it() {
    let env = env().await;
    env.fake.state.lock().fail_create = Some("output path is a symlink".into());
    let service = env.server();
    let error = service.create(&invocation("echo hi", 0), Metadata::new(), &env.placement(&env.directory)).await.unwrap_err();
    assert_eq!(error, "output path is a symlink");
    assert!(service.list().is_empty());
}

#[tokio::test]
async fn spawns_when_the_daemon_is_unavailable_or_the_location_is_remote() {
    let env = env().await;
    let service = env.server();
    // A remote workspace never uses the daemon.
    let mut remote = env.placement(&env.directory);
    remote.workspace_id = Some("wrk_1".into());
    let mut run = invocation("echo remote", 0);
    run.cwd = env.directory.clone();
    let info = service.create(&run, Metadata::new(), &remote).await.unwrap();
    assert_eq!(service.wait(&info.id).await.unwrap().exit, Some(0));
    assert!(env.created().is_empty());
    // A daemon that cannot be reached: spawned instead, once the daemon says so.
    env.fake.close();
    let fresh = PersistentPty::new(
        &{
            let mut g = oc_util::Global::resolve();
            g.state = env.data.join("state2");
            g
        },
        Options { root: Some(env.data.join("root2")), binary: Some(no_spawn()), ..Default::default() },
    )
    .await;
    let spawning = ShellService::new(env.data.clone(), None);
    spawning.attach_daemon(fresh);
    let info = spawning.create(&invocation("echo spawned", 0), Metadata::new(), &env.placement(&env.directory)).await.unwrap();
    let result = spawning.result(&info, 2000, 51200).await;
    assert_eq!(result.capture.unwrap().output, "spawned\n");
    assert_eq!(result.info.cwd, env.directory);
}

#[tokio::test]
async fn stops_a_timed_out_command_with_sigterm_then_sigkill_after_the_grace_period_and_keeps_it() {
    let env = env().await;
    env.fake.state.lock().trap.insert(1);
    let service = env.server();
    let info = create(&service, &env, "trap '' TERM; sleep 60", 10).await;
    until(|| !env.fake.signals(1).is_empty()).await;
    assert_eq!(env.fake.signals(1), vec!["SIGTERM"]);
    assert_eq!(service.get(&info.id).unwrap().status, ShellStatus::Running);
    assert_eq!(service.wait(&info.id).await.unwrap().status, ShellStatus::Timeout);
    assert_eq!(env.fake.signals(1), vec!["SIGTERM", "SIGKILL"]);
    let metadata = env.metadata(1);
    assert_eq!(metadata["stopReason"], "timeout");
    assert!(metadata.get("timeoutDeadline").is_none());
    let result = service.result(&info, 2000, 51200).await;
    assert_eq!(result.info.status, ShellStatus::Timeout);
    assert_eq!(result.info.signal.as_deref(), Some("SIGKILL"));
    assert_eq!(result.capture.unwrap().output, "(no output)");
    assert!(env.fake.ops("terminate").is_empty());
    assert!(Path::new(&info.file).exists());
}

#[tokio::test]
async fn forgets_a_command_in_the_daemon_before_deleting_its_file_on_remove_and_on_eviction() {
    let env = env().await;
    let service = env.server();
    let running = create(&service, &env, "sleep 60", 0).await;
    service.remove(&running.id).unwrap();
    until(|| !Path::new(&running.file).exists()).await;
    assert_eq!(env.fake.ops("terminate").iter().map(|r| r["id"].clone()).collect::<Vec<_>>(), vec![json!(1)]);
    assert!(env.fake.process(1).is_none());
    let result = service.result(&running, 2000, 51200).await;
    assert_eq!((result.info.status, result.capture.is_none()), (ShellStatus::Killed, true));
    // Every shell reports exactly one exit: removing a running one ends it as killed, then deletes it.
    {
        let events = env.events.lock();
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                ShellEvent::Created { .. } => "created",
                ShellEvent::Exited { status: ShellStatus::Killed, .. } => "killed",
                ShellEvent::Exited { .. } => "exited",
                ShellEvent::Deleted { .. } => "deleted",
            })
            .collect();
        assert_eq!(kinds, vec!["created", "killed", "deleted"]);
    }

    let mut finished = Vec::new();
    for index in 0..26u64 {
        let info = create(&service, &env, &format!("echo {index}"), 0).await;
        env.fake.exit(index + 2, Some(0), None);
        service.wait(&info.id).await.unwrap();
        finished.push(info);
    }
    // Waiters resolve before retention runs, which forgets the process and then deletes its file.
    until(|| !Path::new(&finished[0].file).exists()).await;
    assert!(service.get(&finished[0].id).is_err());
    assert!(env.fake.process(2).is_none());
    assert!(Path::new(&finished[1].file).exists());
}

#[tokio::test]
async fn takes_its_commands_back_after_a_restart_and_leaves_other_locations_alone() {
    let env = env().await;
    env.fake.state.lock().trap.insert(4);
    let (running, exited, expiring, stopping, waited) = {
        let service = env.server();
        let tagged: Metadata = json!({"tag": "a"}).as_object().cloned().unwrap();
        let running = service.create(&invocation("running", 0), tagged, &env.placement(&env.directory)).await.unwrap();
        let finished = create(&service, &env, "finished", 0).await;
        env.fake.output(2, "done\n");
        env.fake.exit(2, Some(3), None);
        let exited = within("wait finished", service.wait(&finished.id)).await.unwrap();
        let expiring = create(&service, &env, "expiring", 60_000).await;
        let stopping = create(&service, &env, "stopping", 0).await;
        service.stop(&stopping.id, ShellStatus::Killed).unwrap();
        until(|| !env.fake.signals(4).is_empty()).await;
        // A call waits on this one: a restart moves it to the background, so its timeout is not kept.
        let mut placement = env.placement(&env.directory);
        placement.waited = true;
        placement.tool_call = Some(ToolCall { session_id: "ses_1".into(), assistant_message_id: "msg_1".into(), call_id: "call_1".into() });
        let waited = service.create(&invocation("waited", 60_000), Metadata::new(), &placement).await.unwrap();
        until(|| env.metadata(4)["stopReason"] == "killed").await;
        (running, exited, expiring, stopping, waited)
    };
    // The previous server only detached from its commands.
    assert!(env.fake.ops("terminate").is_empty());
    assert!(env.fake.signals(1).is_empty());
    assert_eq!(env.fake.signals(4), vec!["SIGTERM"]);
    // The expiring command's deadline passes while no server runs.
    {
        let mut state = env.fake.state.lock();
        let process = state.processes.iter_mut().find(|p| p.id() == 3).unwrap();
        process.info["pipe"]["metadata"]["timeoutDeadline"] = json!(crate::util::now_ms() - 1);
    }

    let other = tempfile::tempdir().unwrap();
    let service = env.server();
    let elsewhere = other.path().to_string_lossy().into_owned();
    within("recover elsewhere", service.recover(&elsewhere, None, "proj")).await;
    assert!(service.get(&running.id).is_err());
    assert!(service.list().is_empty());

    within("recover", service.recover(&env.directory, None, "proj")).await;
    let taken = service.get(&running.id).unwrap();
    assert_eq!((taken.status, taken.command.as_str(), taken.pid), (ShellStatus::Running, "running", running.pid));
    assert_eq!((taken.file.as_str(), taken.metadata.get("tag")), (running.file.as_str(), Some(&json!("a"))));
    assert_eq!(service.get(&exited.id).unwrap(), exited);
    assert_eq!(service.result(&exited, 2000, 51200).await.capture.unwrap().output, "done\n");
    // The deadline passed during the restart, and the interrupted stop goes straight to SIGKILL.
    assert_eq!(within("wait expiring", service.wait(&expiring.id)).await.unwrap().status, ShellStatus::Timeout);
    assert_eq!(env.fake.signals(3), vec!["SIGTERM"]);
    assert_eq!(within("wait stopping", service.wait(&stopping.id)).await.unwrap().status, ShellStatus::Killed);
    assert_eq!(env.fake.signals(4), vec!["SIGTERM", "SIGKILL"]);
    assert!(env.metadata(5).get("timeoutDeadline").is_none());
    assert_eq!(service.get(&waited.id).unwrap().status, ShellStatus::Running);
    assert!(env.fake.signals(5).is_empty());
    // The call that started it can be found again.
    let call = ToolCall { session_id: "ses_1".into(), assistant_message_id: "msg_1".into(), call_id: "call_1".into() };
    assert_eq!(service.by_tool_call(&call).map(|i| i.id), Some(waited.id.clone()));
    assert_eq!(service.location_of(&waited.id).map(|l| l.directory), Some(env.directory.clone()));

    env.fake.exit(1, Some(0), None);
    let ended = within("wait running", service.wait(&running.id)).await.unwrap();
    assert_eq!((ended.status, ended.exit), (ShellStatus::Exited, Some(0)));
    // Its exit reports its location, though this server never created it.
    until(|| {
        env.events.lock().iter().any(|e| matches!(e, ShellEvent::Exited { id, location: Some(l), .. } if *id == running.id && l.directory == env.directory))
    })
    .await;
    // Taking a location back happens once per server.
    let lists = env.fake.ops("list").len();
    service.recover(&env.directory, None, "proj").await;
    assert_eq!(env.fake.ops("list").len(), lists);
}

#[tokio::test]
async fn the_sweep_keeps_a_running_commands_file_and_deletes_a_finished_ones_file_and_forgets_it() {
    let env = env().await;
    let (running, finished, lost_file, old) = {
        let service = env.server();
        let running = create(&service, &env, "running", 0).await;
        let mut done = Vec::new();
        for (id, command) in [(2, "finished"), (3, "lost file"), (4, "old")] {
            let info = create(&service, &env, command, 0).await;
            env.fake.exit(id, Some(0), None);
            service.wait(&info.id).await.unwrap();
            done.push(info);
        }
        let old = done.pop().unwrap();
        let lost = done.pop().unwrap();
        (running, done.pop().unwrap(), lost, old)
    };
    // A finished process whose file was deleted elsewhere, and one that ended more than 7 days ago.
    std::fs::remove_file(&lost_file.file).unwrap();
    let eight_days = 8 * 24 * 60 * 60 * 1000;
    {
        let mut state = env.fake.state.lock();
        let process = state.processes.iter_mut().find(|p| p.id() == 4).unwrap();
        process.info["exited_at"] = json!(crate::util::now_ms() - eight_days);
    }
    let expired = std::time::SystemTime::now() - Duration::from_millis(eight_days);
    let set = |path: &str| std::fs::File::options().write(true).open(path).unwrap().set_modified(expired).unwrap();
    set(&running.file);
    set(&finished.file);
    let orphan = Path::new(&running.file).with_file_name("sh_0123456789abORPHANORPHANOR.out");
    std::fs::write(&orphan, "orphan").unwrap();
    std::fs::File::options().write(true).open(&orphan).unwrap().set_modified(expired).unwrap();

    env.server().cleanup_all().await;

    assert!(Path::new(&running.file).exists());
    assert!(!Path::new(&finished.file).exists());
    assert!(!orphan.exists());
    let ids: Vec<u64> = env.fake.state.lock().processes.iter().map(|p| p.id()).collect();
    assert_eq!(ids, vec![1]);
    // The old process's file is recent, so it stays (the process is forgotten).
    assert!(Path::new(&old.file).exists());
}
