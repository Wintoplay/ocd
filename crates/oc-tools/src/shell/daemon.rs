//! Shell commands run by the process daemon (`core/src/shell.ts` `daemonLayer`, 2.3.1).
//!
//! A local command (a location without a workspace) runs in the `opencode-pty` daemon as a pipe process when the
//! daemon's binary is available, so it outlives a planned server restart: the next server takes it back. The daemon
//! writes and counts the output file; what a server needs to take the command back (its shell ID, command, location,
//! tool call, timeout deadline and stop reason) is stored with the process as its metadata (`Stored`).
//!
//! The commands belong to the server, not to the location that started them, grouped by location (`Place`). The
//! first use of a location in a server takes its commands back from the daemon (`recover`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use oc_persistent_pty::{CreatePipeInput, PersistentPty, PipeError, PipeExit, PipeInfo, PipeLifecycle, Signal};
use oc_schema::Metadata;
use oc_schema::message::{ShellInfo, ShellStatus, ShellTime};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::AbortHandle;

use super::service::{EXITED_LIMIT, EventSink, NotFound, ShellEvent, ShellLocation};

/// `STOP_GRACE`: SIGTERM, then SIGKILL this long after.
pub const STOP_GRACE: Duration = Duration::from_secs(3);
/// `LOST`: what a command reports when the daemon went away (and its processes with it) or forgot it.
pub const LOST: &str = "Command was killed: the process daemon stopped";
/// `COUNT_NEWLINES_LIMIT`: the largest file whose lines a result counts when the daemon did not report them.
pub const COUNT_NEWLINES_LIMIT: u64 = 16 * 1024 * 1024;

/// The tool call that started a command, so a server that took it over can settle it (`Shell.ToolCall`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// Where a command runs and who waits on it (`Shell.create` input beyond the invocation).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Placement {
    /// The location's project (its output directory `<data>/shell/<projectID>`).
    pub project_id: String,
    /// The location's directory; relative working directories resolve against it.
    pub directory: String,
    pub workspace_id: Option<String>,
    pub tool_call: Option<ToolCall>,
    /// A call waits on the command. A server that takes the command over moves it to the background, which clears
    /// its timeout, so its deadline is not kept for a restart.
    pub waited: bool,
}

/// `Stored`: what a server needs to take a daemon-run command back after a restart, kept in the daemon's metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Stored {
    id: String,
    command: String,
    shell: String,
    location: StoredLocation,
    metadata: Metadata,
    #[serde(rename = "toolCall", default, skip_serializing_if = "Option::is_none")]
    tool_call: Option<ToolCall>,
    #[serde(rename = "timeoutDeadline", default, skip_serializing_if = "Option::is_none", deserialize_with = "js_millis")]
    timeout_deadline: Option<u64>,
    #[serde(rename = "stopReason", default, skip_serializing_if = "Option::is_none")]
    stop_reason: Option<StopReason>,
}

/// A JS millisecond timestamp (any number).
fn js_millis<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    let value = Option::<f64>::deserialize(deserializer)?;
    Ok(value.map(|v| v.max(0.0) as u64))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredLocation {
    directory: String,
    #[serde(rename = "workspaceID", default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StopReason {
    Timeout,
    Killed,
}

impl StopReason {
    fn status(self) -> ShellStatus {
        match self {
            StopReason::Timeout => ShellStatus::Timeout,
            StopReason::Killed => ShellStatus::Killed,
        }
    }

    fn of(status: ShellStatus) -> StopReason {
        if status == ShellStatus::Timeout { StopReason::Timeout } else { StopReason::Killed }
    }
}

type Done = Option<Result<ShellInfo, NotFound>>;

#[derive(Default)]
struct RunState {
    /// When a restart should stop it (ms since the epoch). Unset while a call waits on it: that call's timeout dies
    /// with this server.
    timeout_deadline: Option<u64>,
    stop_reason: Option<StopReason>,
    /// From the exit: newlines in the file, and why the daemon stopped writing it early.
    output_lines: Option<u64>,
    output_error: Option<String>,
    /// Why it was killed without a signal of its own.
    killed: Option<String>,
    timeout_task: Option<AbortHandle>,
    stopping: Option<AbortHandle>,
    wait: Option<AbortHandle>,
}

/// `Run`: a command the process daemon runs.
pub struct Run {
    info: Mutex<ShellInfo>,
    process: u64,
    tool_call: Option<ToolCall>,
    state: Mutex<RunState>,
    done: watch::Sender<Done>,
}

impl Run {
    pub fn info(&self) -> ShellInfo {
        self.info.lock().clone()
    }

    pub fn done(&self) -> watch::Receiver<Done> {
        self.done.subscribe()
    }

    pub fn file(&self) -> PathBuf {
        PathBuf::from(&self.info.lock().file)
    }

    /// The exit's line count, when the daemon reported it.
    pub fn output_lines(&self) -> Option<u64> {
        self.state.lock().output_lines
    }

    pub fn output_error(&self) -> Option<String> {
        self.state.lock().output_error.clone()
    }

    pub fn killed(&self) -> Option<String> {
        self.state.lock().killed.clone()
    }

    fn running(&self) -> bool {
        self.info.lock().status == ShellStatus::Running
    }
}

/// One location's daemon commands.
pub struct Place {
    pty: PersistentPty,
    events: Option<EventSink>,
    directory: String,
    workspace_id: Option<String>,
    output_dir: PathBuf,
    runs: Mutex<IndexMap<String, Arc<Run>>>,
    exit_order: Mutex<Vec<String>>,
    recovered: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,
}

/// The process daemon's shell commands, owned by the server and grouped by location (`Shell.Daemon`).
pub struct Daemon {
    pty: PersistentPty,
    data: PathBuf,
    events: Option<EventSink>,
    places: Mutex<HashMap<String, Arc<Place>>>,
}

fn now_ms() -> u64 {
    crate::util::now_ms()
}

/// Runs `future` on the current runtime, if any (a drop during shutdown may have none).
fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(future: F) -> Option<AbortHandle> {
    tokio::runtime::Handle::try_current().ok().map(|runtime| runtime.spawn(future).abort_handle())
}

impl Daemon {
    pub fn new(pty: PersistentPty, data: PathBuf, events: Option<EventSink>) -> Arc<Self> {
        Arc::new(Daemon { pty, data, events, places: Mutex::new(HashMap::new()) })
    }

    pub fn pty(&self) -> &PersistentPty {
        &self.pty
    }

    /// Whether commands can run in the daemon now.
    pub fn available(&self) -> bool {
        self.pty.pipes_available()
    }

    /// `handoffPrepared`: the server is handing its daemon commands over to the next one.
    pub fn handoff_prepared(&self) -> bool {
        self.pty.handoff_prepared()
    }

    /// `at(location)`: a location's commands. The first call for a location in this server takes its commands back
    /// from the daemon; every caller waits for that.
    pub async fn at(self: &Arc<Self>, directory: &str, workspace_id: Option<&str>, project_id: &str) -> Arc<Place> {
        let key = format!("{directory}\0{}", workspace_id.unwrap_or_default());
        let place = {
            let mut places = self.places.lock();
            places
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(Place {
                        pty: self.pty.clone(),
                        events: self.events.clone(),
                        directory: directory.to_string(),
                        workspace_id: workspace_id.map(str::to_string),
                        output_dir: self.data.join(super::service::DIRECTORY).join(project_id),
                        runs: Mutex::new(IndexMap::new()),
                        exit_order: Mutex::new(Vec::new()),
                        recovered: Mutex::new(None),
                    })
                })
                .clone()
        };
        // The recovery runs on its own task, so a caller's interrupt never leaves the place half recovered.
        let recovered = place
            .recovered
            .lock()
            .get_or_insert_with(|| {
                let place = place.clone();
                let task = tokio::spawn(async move { place.recover().await });
                async move {
                    let _ = task.await;
                }
                .boxed()
                .shared()
            })
            .clone();
        recovered.await;
        place
    }

    /// Every place known to this server.
    pub fn places(&self) -> Vec<Arc<Place>> {
        self.places.lock().values().cloned().collect()
    }

    /// The command with this shell ID, and its place.
    pub fn find(&self, id: &str) -> Option<(Arc<Place>, Arc<Run>)> {
        self.places().into_iter().find_map(|place| {
            let run = place.runs.lock().get(id).cloned()?;
            Some((place, run))
        })
    }

    /// `byToolCall`: the command a tool call started, while it is known here (including one taken back after a
    /// restart).
    pub fn by_tool_call(&self, call: &ToolCall) -> Option<ShellInfo> {
        self.places()
            .into_iter()
            .find_map(|place| place.runs.lock().values().find(|run| run.tool_call.as_ref() == Some(call)).map(|run| run.info()))
    }

    /// `Shell.cleanup` over the daemon: never starts it. A command it still runs keeps its file; a finished one whose
    /// file expired is deleted, and a finished process whose file is gone, or that ended long ago, is forgotten.
    pub async fn cleanup(&self, retention: Duration) {
        let processes: HashMap<String, PipeInfo> = match self.pty.list_pipes().await {
            Ok(list) => list.into_iter().map(|p| (p.output_path.clone(), p)).collect(),
            Err(_) => HashMap::new(),
        };
        let files: Vec<PathBuf> = super::service::ShellService::output_files(&self.data)
            .into_iter()
            .filter(|file| {
                processes
                    .get(&file.to_string_lossy().into_owned())
                    .is_none_or(|p| p.lifecycle != PipeLifecycle::Running)
            })
            .collect();
        crate::tool_output::retention_cleanup(&files, retention);
        let cutoff = now_ms().saturating_sub(retention.as_millis() as u64);
        for process in processes.values() {
            if process.lifecycle == PipeLifecycle::Running {
                continue;
            }
            let stale = process.exited_at.unwrap_or(process.started_at) < cutoff;
            let exists = match std::fs::symlink_metadata(&process.output_path) {
                Ok(_) => true,
                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
            };
            if !stale && exists {
                continue;
            }
            let _ = self.pty.terminate_pipe(process.id).await;
        }
    }
}

/// The input of [`Place::create`].
pub struct CreateRun {
    pub info: ShellInfo,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub timeout: u64,
    pub tool_call: Option<ToolCall>,
    pub waited: bool,
}

impl Place {
    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn location(&self) -> ShellLocation {
        ShellLocation { directory: self.directory.clone(), workspace_id: self.workspace_id.clone() }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Run>> {
        self.runs.lock().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<Run>> {
        self.runs.lock().values().cloned().collect()
    }

    fn emit(&self, event: ShellEvent) {
        if let Some(sink) = &self.events {
            sink(event);
        }
    }

    fn persisted(&self, run: &Run) -> Metadata {
        let info = run.info.lock().clone();
        let state = run.state.lock();
        let stored = Stored {
            id: info.id,
            command: info.command,
            shell: info.shell,
            location: StoredLocation { directory: self.directory.clone(), workspace_id: self.workspace_id.clone() },
            metadata: info.metadata,
            tool_call: run.tool_call.clone(),
            timeout_deadline: state.timeout_deadline,
            stop_reason: state.stop_reason,
        };
        match serde_json::to_value(stored) {
            Ok(Value::Object(map)) => map,
            _ => Metadata::new(),
        }
    }

    /// Records the command's latest timeout and stop reason in the daemon, so a restart sees them.
    async fn store(&self, run: &Run) {
        if let Err(error) = self.pty.set_pipe_metadata(run.process, self.persisted(run)).await {
            tracing::warn!(id = %run.info.lock().id, error = %error, "failed to update shell command metadata");
        }
    }

    /// `remove(id)`: forgets the command in the daemon (killing it if it still runs) before deleting its file.
    pub async fn remove(self: &Arc<Self>, id: &str) {
        self.exit_order.lock().retain(|x| x != id);
        let Some(run) = self.runs.lock().shift_remove(id) else { return };
        {
            let mut state = run.state.lock();
            // Stop waiting first, so forgetting the process is not mistaken for losing it.
            for task in [state.timeout_task.take(), state.stopping.take(), state.wait.take()].into_iter().flatten() {
                task.abort();
            }
        }
        let _ = self.pty.terminate_pipe(run.process).await;
        let _ = std::fs::remove_file(run.file());
        run.done.send_if_modified(|done| {
            if done.is_none() {
                *done = Some(Err(NotFound { id: id.to_string() }));
                true
            } else {
                false
            }
        });
        // Every shell reports exactly one exit; removing a running one ends it as killed.
        if run.running() {
            self.emit(ShellEvent::Exited { id: id.to_string(), exit: None, status: ShellStatus::Killed, location: Some(self.location()) });
        }
        self.emit(ShellEvent::Deleted { id: id.to_string(), location: Some(self.location()) });
    }

    async fn evict(self: &Arc<Self>) {
        loop {
            let oldest = {
                let order = self.exit_order.lock();
                if order.len() <= EXITED_LIMIT {
                    return;
                }
                order[0].clone()
            };
            self.remove(&oldest).await;
        }
    }

    async fn finish(self: &Arc<Self>, run: &Arc<Run>, status: ShellStatus, exit: Option<i64>, signal: Option<String>, completed: u64) {
        let id = run.info.lock().id.clone();
        // A removed command already failed its waiters and published its exit and deletion.
        if !self.runs.lock().get(&id).is_some_and(|current| Arc::ptr_eq(current, run)) {
            return;
        }
        let info = {
            let mut info = run.info.lock();
            info.status = status;
            if exit.is_some() {
                info.exit = exit;
            }
            if signal.is_some() {
                info.signal = signal;
            }
            info.time.completed = Some(completed);
            info.clone()
        };
        if let Some(task) = run.state.lock().timeout_task.take() {
            task.abort();
        }
        // Resolve waiters before retention eviction, so an evicted command still reports its terminal info rather
        // than the removal's not-found.
        run.done.send_if_modified(|done| {
            if done.is_none() {
                *done = Some(Ok(info.clone()));
                true
            } else {
                false
            }
        });
        self.emit(ShellEvent::Exited { id: id.clone(), exit: info.exit, status, location: Some(self.location()) });
        self.exit_order.lock().push(id);
        self.evict().await;
    }

    /// `stop(run, reason)`: graceful, like spawned commands: SIGTERM, then SIGKILL if it has not exited 3 s later.
    /// The stop is recorded before this returns; the SIGTERM and the escalation run on their own, so neither
    /// interrupting the caller nor a server shutting down right after cancels them.
    pub fn stop(self: &Arc<Self>, run: &Arc<Run>, reason: ShellStatus) {
        {
            let mut state = run.state.lock();
            if state.stop_reason.is_some() || !run.running() {
                return;
            }
            state.stop_reason = Some(StopReason::of(reason));
            state.timeout_deadline = None;
        }
        let place = self.clone();
        let task_run = run.clone();
        let handle = spawn(async move {
            let run = task_run;
            place.store(&run).await;
            let _ = place.pty.signal_pipe(run.process, Signal::Term).await;
            let mut done = run.done();
            let ended = tokio::time::timeout(STOP_GRACE, done.wait_for(|done| done.is_some())).await.is_ok();
            if !ended {
                let _ = place.pty.signal_pipe(run.process, Signal::Kill).await;
            }
        });
        run.state.lock().stopping = handle;
    }

    fn arm(self: &Arc<Self>, run: &Arc<Run>, duration: u64) {
        let place = self.clone();
        let task_run = run.clone();
        let handle = spawn(async move {
            tokio::time::sleep(Duration::from_millis(duration)).await;
            place.stop(&task_run, ShellStatus::Timeout);
        });
        run.state.lock().timeout_task = handle;
    }

    /// `timeout(run, duration)`: replaces the timeout from now; zero clears it. Ignored once the command ended or is
    /// stopping.
    pub fn timeout(self: &Arc<Self>, run: &Arc<Run>, duration: u64) {
        {
            let mut state = run.state.lock();
            if state.stop_reason.is_some() || !run.running() {
                return;
            }
            if let Some(task) = state.timeout_task.take() {
                task.abort();
            }
            state.timeout_deadline = (duration != 0).then(|| now_ms() + duration);
        }
        let place = self.clone();
        let task_run = run.clone();
        spawn(async move { place.store(&task_run).await });
        if duration != 0 {
            self.arm(run, duration);
        }
    }

    /// `attach(run, duration)`: watches the exit, and the timeout (in ms from now, none if `None`).
    fn attach(self: &Arc<Self>, run: &Arc<Run>, duration: Option<u64>) {
        let place = self.clone();
        let task_run = run.clone();
        let wait = spawn(async move {
            let run = task_run;
            let (status, exit, signal, completed) = match place.pty.wait_pipe(run.process).await {
                Ok(PipeExit { exit_code, signal, output_lines, output_error, exited_at, .. }) => {
                    let mut state = run.state.lock();
                    state.output_lines = Some(output_lines);
                    state.output_error = output_error;
                    // A stopped command reports its reason, and how the stopped process ended, as spawned ones do.
                    let status = state.stop_reason.map_or(ShellStatus::Exited, StopReason::status);
                    (status, exit_code.map(|c| c as i64), signal, exited_at)
                }
                // The daemon went away (and its processes with it), or forgot the process: it was killed.
                Err(error) => {
                    tracing::warn!(id = %run.info.lock().id, error = %error, "lost a shell command's process");
                    run.state.lock().killed = Some(LOST.to_string());
                    (ShellStatus::Killed, None, None, now_ms())
                }
            };
            place.finish(&run, status, exit, signal, completed).await;
        });
        run.state.lock().wait = wait;
        // A stop interrupted by a restart lost its grace period: kill now.
        if run.state.lock().stop_reason.is_some() {
            let pty = self.pty.clone();
            let process = run.process;
            let stopping = spawn(async move {
                let _ = pty.signal_pipe(process, Signal::Kill).await;
            });
            run.state.lock().stopping = stopping;
            return;
        }
        match duration {
            None => {}
            Some(duration) if duration > 0 => self.arm(run, duration),
            Some(_) => self.stop(run, ShellStatus::Timeout),
        }
    }

    /// `create(input)`: runs the command in the daemon. Fails with [`PipeError::Unavailable`] only when the daemon
    /// could not be reached or started: spawning instead is safe.
    pub async fn create(self: &Arc<Self>, input: CreateRun) -> Result<ShellInfo, PipeError> {
        let (done, _) = watch::channel(None);
        let deadline = (!input.waited && input.timeout > 0).then(|| input.info.time.started + input.timeout);
        let mut run = Run {
            info: Mutex::new(input.info.clone()),
            process: 0,
            tool_call: input.tool_call,
            state: Mutex::new(RunState { timeout_deadline: deadline, ..Default::default() }),
            done,
        };
        let created = self
            .pty
            .create_pipe(CreatePipeInput {
                program: input.info.shell.clone(),
                args: input.args,
                cwd: input.info.cwd.clone(),
                env: input.env,
                group_id: self.output_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                output_path: input.info.file.clone(),
                metadata: self.persisted(&run),
            })
            .await?;
        run.process = created.id;
        {
            let mut info = run.info.lock();
            if let Some(pid) = created.pid {
                info.pid = Some(pid as u32);
            }
            // The daemon's clock, so the command reports the same start wherever it is read.
            info.time.started = created.started_at;
        }
        let run = Arc::new(run);
        let info = run.info();
        self.runs.lock().insert(info.id.clone(), run.clone());
        // Timed from now: starting the daemon may have taken a while.
        self.attach(&run, (input.timeout > 0).then_some(input.timeout));
        self.emit(ShellEvent::Created { info: info.clone(), location: Some(self.location()) });
        Ok(info)
    }

    /// `recover()`: takes back this location's commands, left by a previous server. Two locations can share a
    /// project's output directory, so the location itself must match.
    async fn recover(self: Arc<Self>) {
        let processes = match self.pty.list_pipes().await {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(error = %error, "failed to list shell commands to recover");
                Vec::new()
            }
        };
        let mut finished: Vec<Arc<Run>> = Vec::new();
        for process in processes {
            let Ok(stored) = serde_json::from_value::<Stored>(Value::Object(process.metadata.clone())) else { continue };
            if stored.location.directory != self.directory
                || stored.location.workspace_id != self.workspace_id
                || Path::new(&process.output_path).parent() != Some(self.output_dir.as_path())
                || self.runs.lock().contains_key(&stored.id)
            {
                continue;
            }
            let running = process.lifecycle == PipeLifecycle::Running;
            let (exit, signal) = match &process.lifecycle {
                PipeLifecycle::Exited { exit_code, signal } => (exit_code.map(|c| c as i64), signal.clone()),
                _ => (None, None),
            };
            let status = if running {
                ShellStatus::Running
            } else {
                match (stored.stop_reason, &process.lifecycle) {
                    (Some(reason), _) => reason.status(),
                    (None, PipeLifecycle::Exited { .. }) => ShellStatus::Exited,
                    (None, _) => ShellStatus::Killed,
                }
            };
            let killed = match (&process.lifecycle, stored.stop_reason) {
                (PipeLifecycle::Failed { message }, None) => Some(format!("Command was killed: {message}")),
                _ => None,
            };
            let info = ShellInfo {
                id: stored.id.clone(),
                status,
                command: stored.command.clone(),
                cwd: process.cwd.clone(),
                shell: stored.shell.clone(),
                file: process.output_path.clone(),
                pid: process.pid.map(|p| p as u32),
                exit,
                signal,
                metadata: stored.metadata.clone(),
                time: ShellTime {
                    started: process.started_at,
                    completed: (!running).then(|| process.exited_at.unwrap_or(process.started_at)),
                },
            };
            let (done, _) = watch::channel(None);
            let run = Arc::new(Run {
                info: Mutex::new(info.clone()),
                process: process.id,
                tool_call: stored.tool_call.clone(),
                state: Mutex::new(RunState {
                    timeout_deadline: stored.timeout_deadline,
                    stop_reason: stored.stop_reason,
                    output_lines: (!running).then_some(process.output_lines),
                    output_error: if running { None } else { process.output_error.clone() },
                    killed,
                    ..Default::default()
                }),
                done,
            });
            self.runs.lock().insert(stored.id.clone(), run.clone());
            if running {
                // A deadline that passed while no server ran fires at once.
                let duration = stored.timeout_deadline.map(|deadline| deadline.saturating_sub(now_ms()));
                self.attach(&run, duration);
                continue;
            }
            run.done.send_replace(Some(Ok(info)));
            finished.push(run);
        }
        finished.sort_by_key(|run| run.info.lock().time.completed.unwrap_or(0));
        self.exit_order.lock().extend(finished.iter().map(|run| run.info.lock().id.clone()));
        self.evict().await;
    }
}
