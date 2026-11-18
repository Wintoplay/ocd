//! Shell commands on the real `opencode-pty` daemon, on every platform (ignored by default: needs the binary,
//! from `OPENCODE_PTY_BIN` or downloaded from npm). A command runs in the daemon with the invocation's environment,
//! a stop ends it with the signal the daemon sent, and a second server takes a running command back through the
//! restart handoff and sees it finish.
//!
//! Run: `cargo test -p oc-tools --test daemon_real -- --ignored --nocapture`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use oc_persistent_pty::{BinaryFn, BinaryResolver, Options, PersistentPty};
use oc_schema::Metadata;
use oc_schema::message::ShellStatus;
use oc_tools::host::ShellInvocation;
use oc_tools::shell::daemon::Placement;
use oc_tools::shell::service::ShellService;

async fn binary() -> String {
    if let Ok(path) = std::env::var("OPENCODE_PTY_BIN") {
        return path;
    }
    let bin = std::env::temp_dir().join("oc-persistent-pty-test-bin");
    let resolver = BinaryResolver::new(bin, reqwest::Client::new());
    let package = oc_persistent_pty::binary::current_package().expect("supported platform");
    resolver.install(package).await.expect("install opencode-pty").to_string_lossy().into_owned()
}

fn fixed(path: String) -> BinaryFn {
    Arc::new(move || {
        let path = path.clone();
        Box::pin(async move { Ok(path) })
    })
}

/// A shell and commands for it: PowerShell on Windows, `/bin/sh` elsewhere.
fn shell() -> String {
    if cfg!(windows) {
        oc_tools::shell::select::resolve(oc_tools::shell::select::Priority::Compat, None, None)
    } else {
        "/bin/sh".into()
    }
}

fn echo(text: &str) -> String {
    if cfg!(windows) { format!("Write-Output \"{text} $env:OCD_DAEMON_TEST\"") } else { format!("echo {text} $OCD_DAEMON_TEST") }
}

fn sleep_then(seconds: u32, text: &str) -> String {
    if cfg!(windows) {
        format!("Start-Sleep -Seconds {seconds}; Write-Output {text}")
    } else {
        format!("sleep {seconds}; echo {text}")
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn shell_commands_run_in_the_real_daemon_and_survive_a_handoff() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let directory = root_path.join("project");
    std::fs::create_dir_all(&directory).unwrap();
    let directory = directory.to_string_lossy().into_owned();
    let mut global = oc_util::Global::resolve();
    global.state = root_path.join("state");
    global.bin = root_path.join("bin");
    let exe = binary().await;
    let first = PersistentPty::new(&global, Options { binary: Some(fixed(exe.clone())), ..Default::default() }).await;
    assert!(first.pipes_available());
    let data = root_path.join("data");
    let service = ShellService::new(data.clone(), None);
    service.attach_daemon(first.clone());

    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.insert("OCD_DAEMON_TEST".into(), "env-ok".into());
    let invocation = |command: String| ShellInvocation { command, cwd: String::new(), timeout: 0, shell: shell(), env: env.clone() };
    let placement = Placement { project_id: "proj".into(), directory: directory.clone(), ..Default::default() };

    // A command runs in the daemon (its pid is the daemon's child) with the invocation's environment.
    let info = service.create(&invocation(echo("hello")), Metadata::new(), &placement).await.unwrap();
    assert!(info.pid.is_some());
    assert_eq!(first.list_pipes().await.unwrap().len(), 1);
    let result = service.result(&info, 2000, 51200).await;
    assert_eq!((result.info.status, result.info.exit), (ShellStatus::Exited, Some(0)), "{result:?}");
    assert_eq!(result.capture.unwrap().output.trim_end(), "hello env-ok");
    assert_eq!(result.info.cwd, directory);

    // A stopped command ends with the daemon's signal.
    let stopped = service.create(&invocation(sleep_then(30, "never")), Metadata::new(), &placement).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    service.stop(&stopped.id, ShellStatus::Killed).unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(20), service.wait(&stopped.id)).await.unwrap().unwrap();
    assert_eq!(ended.status, ShellStatus::Killed);
    assert_eq!(ended.signal.as_deref(), Some("SIGTERM"), "{ended:?}");

    // A running command survives the handoff to the next server, which takes it back and sees it finish.
    let long = service.create(&invocation(sleep_then(3, "survived")), Metadata::new(), &placement).await.unwrap();
    let handoff = first.handoff().await.unwrap().expect("a handoff");
    assert!(first.handoff_prepared());
    first.close();
    drop(service);
    drop(first);
    let second = PersistentPty::new(&global, Options { binary: Some(fixed(exe)), handoff: Some(handoff), ..Default::default() }).await;
    assert!(second.daemon().is_owner());
    let next = ShellService::new(data, None);
    next.attach_daemon(second.clone());
    next.recover(&directory, None, "proj").await;
    let taken = next.get(&long.id).unwrap();
    assert_eq!((taken.status, taken.pid, taken.file.as_str()), (ShellStatus::Running, long.pid, long.file.as_str()));
    let result = tokio::time::timeout(Duration::from_secs(30), next.result(&long, 2000, 51200)).await.unwrap();
    assert_eq!((result.info.status, result.info.exit), (ShellStatus::Exited, Some(0)), "{result:?}");
    assert_eq!(result.capture.unwrap().output.trim_end(), "survived");
    second.shutdown().await.unwrap();
}
