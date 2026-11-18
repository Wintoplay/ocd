//! Client for opencode's persistent terminals (`packages/core/src/persistent-pty/**`).
//!
//! Terminals and pipe processes (shell commands) live in the external `opencode-pty` daemon
//! (npm `@opencode/pty@0.6.0`, protocol 9) so they survive server restarts. [`PersistentPty`] is
//! the service the HTTP `persistentPty.*` endpoints, the TUI terminal pane and the shell service
//! use; see `README.md`.

pub mod binary;
pub mod daemon;
pub mod frame;
pub mod handoff;
#[cfg(all(unix, feature = "testing"))]
pub mod testing;
pub mod types;
pub mod wire;
pub mod ws;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

pub use binary::{AVAILABLE, BinaryResolver, available};
pub use daemon::{BinaryFn, DaemonError, DaemonTransport, DetachHandle, ErrorKind, Replay, SubscribeInput};
pub use handoff::Handoff;
pub use types::{
    BusEvent, CreateInput, Cursor, Info, Output, ReadResult, Screen, Size, Snapshot, Status, UpdateInput, from_id, to_id,
};
pub use wire::{Exited, Lifecycle, ProcessInfo, Role, StreamEvent};

/// Metadata a client stores with a pipe process (`Metadata`).
pub type Metadata = serde_json::Map<String, Value>;

/// Service errors (`PersistentPty.UnavailableError` / `PersistentPty.NotFoundError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Maps to HTTP `ServiceUnavailableError {message, service: "opencode-pty"}`.
    #[error("{0}")]
    Unavailable(String),
    /// Maps to HTTP `PtyNotFoundError {ptyID, message: "PTY session not found: <id>"}`.
    #[error("PTY session not found: {0}")]
    NotFound(String),
    /// `PersistentPty.InvalidCwdError`: maps to HTTP 400 `InvalidRequestError` (`kind: "invalid_cwd"`).
    #[error("{message}")]
    InvalidCwd { cwd: String, message: String },
}

/// Errors of the pipe-process methods.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PipeError {
    /// `PersistentPty.UnavailableError`: the daemon could not be reached or started; the request never ran, so
    /// running the command some other way is safe.
    #[error("{0}")]
    Unavailable(String),
    /// `PersistentPty.RequestError`: the daemon rejected the request, or its reply was lost after it was sent, so
    /// it may have run.
    #[error("{0}")]
    Request(String),
    /// `PersistentPty.ProcessNotFoundError`: an unknown id, a terminal, or no daemon running.
    #[error("opencode-pty process {0} not found")]
    NotFound(u64),
}

/// Only a failure before the request was sent is safe to retry somewhere else (`pipeError`).
fn pipe_error(error: DaemonError) -> PipeError {
    match error.kind {
        ErrorKind::Request | ErrorKind::Response => PipeError::Request(error.message),
        _ => PipeError::Unavailable(error.message),
    }
}

fn unexpected_pipe(response: &wire::WireResponse) -> PipeError {
    PipeError::Request(format!("unexpected opencode-pty response: {}", response.type_name()))
}

/// A signal for a pipe process (`Signal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Int,
    Kill,
}

impl Signal {
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Term => "SIGTERM",
            Signal::Int => "SIGINT",
            Signal::Kill => "SIGKILL",
        }
    }
}

/// How a pipe process stands (`PipeInfo.lifecycle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeLifecycle {
    Running,
    Exited { exit_code: Option<u64>, signal: Option<String> },
    Failed { message: String },
}

/// A non-PTY process run by the daemon, its output written to `output_path` (`PipeInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeInfo {
    pub id: u64,
    pub pid: Option<u64>,
    pub command: Vec<String>,
    pub cwd: String,
    pub group_id: String,
    pub lifecycle: PipeLifecycle,
    /// Bytes counted so far; the file can hold a little more. Pair only bytes up to here with `output_lines`.
    pub output_tail: u64,
    pub output_lines: u64,
    pub output_path: String,
    pub output_error: Option<String>,
    pub metadata: Metadata,
    pub started_at: u64,
    pub exited_at: Option<u64>,
}

impl PipeInfo {
    /// `toPipeInfo`; `None` for a terminal.
    pub fn from_wire(value: &ProcessInfo) -> Option<Self> {
        let pipe = value.pipe.as_ref()?;
        Some(PipeInfo {
            id: value.id,
            pid: value.pid,
            command: value.command.clone(),
            cwd: value.cwd.clone(),
            group_id: value.group_id.clone(),
            lifecycle: match &value.lifecycle {
                Lifecycle::Running => PipeLifecycle::Running,
                Lifecycle::Exited { exit_code, signal } => {
                    PipeLifecycle::Exited { exit_code: *exit_code, signal: signal.clone() }
                }
                Lifecycle::Failed { message } => PipeLifecycle::Failed { message: message.clone() },
            },
            output_tail: value.output_tail,
            output_lines: pipe.output_lines,
            output_path: pipe.output_path.clone(),
            output_error: pipe.output_error.clone(),
            metadata: pipe.metadata.clone(),
            started_at: value.started_at,
            exited_at: value.exited_at,
        })
    }
}

/// How a pipe process ended (`PipeExit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeExit {
    pub exit_code: Option<u64>,
    pub signal: Option<String>,
    /// Runs past the file's size when `output_error` is set; use the file's real size instead.
    pub final_offset: u64,
    pub output_lines: u64,
    pub output_error: Option<String>,
    pub exited_at: u64,
}

impl From<Exited> for PipeExit {
    fn from(value: Exited) -> Self {
        PipeExit {
            exit_code: value.exit_code,
            signal: value.signal,
            final_offset: value.final_offset,
            output_lines: value.output_lines.unwrap_or(0),
            output_error: value.output_error,
            exited_at: value.exited_at,
        }
    }
}

/// `CreatePipeInput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePipeInput {
    pub program: String,
    pub args: Vec<String>,
    /// Absolute.
    pub cwd: String,
    /// The child's whole environment; the daemon's own is not inherited.
    pub env: HashMap<String, String>,
    pub group_id: String,
    /// Absolute; truncated on open, symlinks refused.
    pub output_path: String,
    pub metadata: Metadata,
}

/// Node `path.resolve(input)`: joined onto the process cwd when relative, then normalized
/// lexically (no symlinks resolved).
fn resolve_path(input: &str) -> String {
    let path = std::path::Path::new(input);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")).join(path)
    };
    let mut out = std::path::PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                if out.parent().is_some() {
                    out.pop();
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<DaemonError> for Error {
    fn from(error: DaemonError) -> Self {
        Error::Unavailable(error.message)
    }
}

fn unexpected(response: &wire::WireResponse) -> Error {
    Error::Unavailable(format!("unexpected opencode-pty response: {}", response.type_name()))
}

/// Default program for new terminals (`ShellSelect.environment(global.bin)`).
pub type ShellFn = Arc<dyn Fn() -> String + Send + Sync>;

/// `PersistentPty.Options` plus Rust-side injection points.
#[derive(Clone, Default)]
pub struct Options {
    /// Inherited restart handoff (see [`handoff::take_from_env`]).
    pub handoff: Option<Handoff>,
    /// Parent of per-server daemon runtime directories; defaults to `<state>/pty`.
    pub root: Option<PathBuf>,
    /// Override the binary resolver (default: [`BinaryResolver`] over `global.bin`).
    pub binary: Option<BinaryFn>,
    /// Override the default shell (default: [`shell_environment`]).
    pub shell: Option<ShellFn>,
    /// HTTP client for the npm download (default: a new client).
    pub http: Option<reqwest::Client>,
    /// Initial daemon name (default: random UUID).
    pub id: Option<String>,
}

/// Attach parameters (`persistentPty.connect` query).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachInput {
    pub cursor: u64,
    pub attachment_id: String,
    pub role: Role,
    pub takeover: bool,
}

/// A live attachment (`PersistentPty.Attachment`).
pub struct Attachment {
    pub info: Info,
    pub role: Role,
    pub generation: u64,
    pub replay: Replay,
    inner: daemon::DaemonAttachment,
    hook: daemon::EventHook,
}

impl Attachment {
    /// Start delivering live events (`activate()`). `None` if already activated or detached.
    /// The stream ends after `exited`, on daemon EOF/error, or after [`Attachment::detach`].
    pub fn activate(&mut self) -> Option<EventStream> {
        self.inner.activate(Some(self.hook.clone())).map(|rx| EventStream { rx })
    }

    /// Destroy the subscription socket (`detach()`).
    pub fn detach(&self) {
        self.inner.detach();
    }

    pub fn detach_handle(&self) -> DetachHandle {
        self.inner.detach_handle()
    }
}

/// Live events of an activated attachment.
pub struct EventStream {
    rx: mpsc::UnboundedReceiver<StreamEvent>,
}

impl EventStream {
    pub async fn recv(&mut self) -> Option<StreamEvent> {
        self.rx.recv().await
    }
}

impl futures::Stream for EventStream {
    type Item = StreamEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        self.rx.poll_recv(cx)
    }
}

struct Inner {
    daemon: DaemonTransport,
    events: broadcast::Sender<BusEvent>,
    /// Controller activity selects a terminal; observer reads and pane visibility do not.
    current: Mutex<HashMap<String, String>>,
    removing: Mutex<HashSet<String>>,
    shell: ShellFn,
    /// When the prepared handoff's ticket expires (ms); 0 when none is prepared.
    handoff_expires_at: AtomicI64,
    /// The resolver behind the default binary; `None` when the binary was injected.
    resolver: Option<BinaryResolver>,
    /// Set once the binary was found without a download.
    installed: std::sync::OnceLock<()>,
    prefetching: std::sync::atomic::AtomicBool,
}

/// The persistent PTY service (`PersistentPty.Service`). Cheap to clone.
#[derive(Clone)]
pub struct PersistentPty {
    inner: Arc<Inner>,
}

impl PersistentPty {
    /// `makeLayer(options)`. Adopts `options.handoff` when given (failures are logged and the
    /// old daemon is abandoned). Never fails; the daemon is spawned lazily on first `create`.
    pub async fn new(global: &oc_util::Global, options: Options) -> Self {
        let root = options.root.clone().unwrap_or_else(|| global.state.join("pty"));
        let resolver = options
            .binary
            .is_none()
            .then(|| BinaryResolver::new(global.bin.clone(), options.http.clone().unwrap_or_else(oc_util::http::client)));
        let binary = options.binary.clone().unwrap_or_else(|| memoized_binary(resolver.clone().expect("default resolver")));
        let shell = options.shell.clone().unwrap_or_else(|| {
            let bin = global.bin.clone();
            Arc::new(move || shell_environment(&bin))
        });
        let id = options.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let daemon = DaemonTransport::new(root, id, binary, options.handoff.clone()).await;
        let (events, _) = broadcast::channel(256);
        PersistentPty {
            inner: Arc::new(Inner {
                daemon,
                events,
                current: Mutex::new(HashMap::new()),
                removing: Mutex::new(HashSet::new()),
                shell,
                handoff_expires_at: AtomicI64::new(0),
                resolver,
                installed: std::sync::OnceLock::new(),
                prefetching: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// `available` (true on every platform since 2.1; reported as `capabilities.persistentPty`).
    pub fn available(&self) -> bool {
        AVAILABLE
    }

    /// Whether pipe processes can run in the daemon without downloading its binary first: a daemon is running, the
    /// binary was injected, or it is installed (see [`BinaryResolver::installed`]). Upstream embeds the binary, so
    /// its `available` is always true; the port downloads it once (on the first terminal, or [`Self::prefetch`]).
    pub fn pipes_available(&self) -> bool {
        if self.inner.installed.get().is_some() || self.inner.daemon.is_owner() {
            return true;
        }
        let found = match &self.inner.resolver {
            None => true,
            Some(resolver) => resolver.installed().is_some(),
        };
        if found {
            let _ = self.inner.installed.set(());
        }
        found
    }

    /// Downloads and installs the pinned binary in the background when it is missing, so later shell commands run
    /// in the daemon. At most once per process; failures are only logged.
    pub fn prefetch(&self) {
        if self.pipes_available() || self.inner.prefetching.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(resolver) = self.inner.resolver.clone() else { return };
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            match resolver.resolve().await {
                Ok(_) => {
                    if let Some(inner) = weak.upgrade() {
                        let _ = inner.installed.set(());
                    }
                }
                Err(error) => tracing::warn!(error = %error, "failed to install opencode-pty"),
            }
        });
    }

    /// `persistent-pty.added` / `persistent-pty.removed` events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<BusEvent> {
        self.inner.events.subscribe()
    }

    /// The underlying transport (runtime directory, ownership state).
    pub fn daemon(&self) -> &DaemonTransport {
        &self.inner.daemon
    }

    /// Scope finalizer: drop the owner connection (the daemon stops unless a handoff was prepared).
    pub fn close(&self) {
        self.inner.daemon.close();
    }

    fn publish(&self, event: BusEvent) {
        let _ = self.inner.events.send(event);
    }

    async fn request(&self, value: Value, start: bool) -> Result<wire::WireResponse> {
        Ok(self.inner.daemon.request(value, start).await?)
    }

    /// `list(sessionID?)`: `[]` when no daemon is running.
    pub async fn list(&self, session_id: Option<&str>) -> Result<Vec<Info>> {
        let Some(response) = self.inner.daemon.request_if_running(json!({ "op": "list", "mode": "pty" })).await? else {
            return Ok(Vec::new());
        };
        let wire::WireResponse::Processes { processes } = &response else { return Err(unexpected(&response)) };
        Ok(processes
            .iter()
            .map(Info::from_wire)
            .filter(|t| session_id.is_none_or(|s| t.session_id == s))
            .collect())
    }

    /// `get(id)`. Any ID that is not one of ours names no terminal; every other method looks the terminal up here
    /// first.
    pub async fn get(&self, id: &str) -> Result<Info> {
        let found = match parse_id(id) {
            Some(parsed) => self.lookup(parsed).await.map_err(|error| Error::Unavailable(error.message))?,
            None => None,
        };
        match found {
            Some(process) if process.mode == wire::Mode::Pty => Ok(Info::from_wire(&process)),
            _ => Err(Error::NotFound(id.to_string())),
        }
    }

    /// `lookup`: `get` fails only for an unknown id, and no running daemon has no processes.
    async fn lookup(&self, id: u64) -> std::result::Result<Option<ProcessInfo>, DaemonError> {
        match self.inner.daemon.request_if_running(json!({ "op": "get", "id": id })).await {
            Ok(None) => Ok(None),
            Ok(Some(wire::WireResponse::Process { process })) => Ok(Some(process)),
            Ok(Some(other)) => Err(DaemonError::new(
                ErrorKind::Response,
                format!("unexpected opencode-pty response: {}", other.type_name()),
            )),
            Err(error) if error.kind == ErrorKind::Request => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn wire_id(id: &str) -> Result<u64> {
        from_id(id).map_err(Error::Unavailable)
    }

    /// `create(sessionID, input)`: spawns the daemon on first use; publishes `persistent-pty.added`.
    pub async fn create(&self, session_id: &str, input: CreateInput) -> Result<Info> {
        // Checked before any daemon request, so a bad cwd never spawns the daemon.
        let cwd = resolve_path(input.cwd.as_deref().unwrap_or("/"));
        if !std::fs::metadata(&cwd).is_ok_and(|info| info.is_dir()) {
            return Err(Error::InvalidCwd { message: format!("cwd is not a directory: {cwd}"), cwd });
        }
        let program = input.command.clone().unwrap_or_else(|| (self.inner.shell)());
        let response = self
            .request(
                json!({
                    "op": "create_pty",
                    "program": program,
                    "args": input.args,
                    "cwd": cwd,
                    "title": input.title,
                    "group_id": session_id,
                    "env": input.env,
                    "cols": input.size.map(|s| s.cols).unwrap_or(80),
                    "rows": input.size.map(|s| s.rows).unwrap_or(24),
                }),
                true,
            )
            .await?;
        let wire::WireResponse::Created { process } = &response else { return Err(unexpected(&response)) };
        let terminal = Info::from_wire(process);
        self.publish(BusEvent::Added { session_id: session_id.to_string(), terminal: terminal.clone() });
        Ok(terminal)
    }

    fn expect_ok(response: wire::WireResponse) -> Result<()> {
        if response == wire::WireResponse::Ok { Ok(()) } else { Err(unexpected(&response)) }
    }

    /// `write(id, data, attachmentID?)`.
    pub async fn write(&self, id: &str, data: &[u8], attachment_id: Option<&str>) -> Result<()> {
        self.get(id).await?;
        let response = self
            .request(
                json!({
                    "op": "write",
                    "id": Self::wire_id(id)?,
                    "attachment_id": attachment_id,
                    "data_base64": wire::encode_base64(data),
                }),
                false,
            )
            .await?;
        Self::expect_ok(response)
    }

    /// `resize(id, cols, rows, attachmentID?)` (`persistentPty.update`); selects the terminal.
    pub async fn resize(&self, id: &str, cols: u64, rows: u64, attachment_id: Option<&str>) -> Result<()> {
        let terminal = self.get(id).await?;
        let response = self
            .request(
                json!({
                    "op": "resize",
                    "id": Self::wire_id(id)?,
                    "attachment_id": attachment_id,
                    "cols": cols,
                    "rows": rows,
                }),
                false,
            )
            .await?;
        Self::expect_ok(response)?;
        self.inner.current.lock().insert(terminal.session_id, id.to_string());
        Ok(())
    }

    /// `control(id, attachmentID, cols, rows)` (framed input type 0); selects the terminal.
    pub async fn control(&self, id: &str, attachment_id: &str, cols: u64, rows: u64) -> Result<()> {
        let terminal = self.get(id).await?;
        let response = self
            .request(
                json!({
                    "op": "control",
                    "id": Self::wire_id(id)?,
                    "attachment_id": attachment_id,
                    "cols": cols,
                    "rows": rows,
                }),
                false,
            )
            .await?;
        Self::expect_ok(response)?;
        self.inner.current.lock().insert(terminal.session_id, id.to_string());
        Ok(())
    }

    /// `input(id, attachmentID, cols, rows, data)` (framed input type 1 / legacy input); selects the terminal.
    pub async fn input(&self, id: &str, attachment_id: &str, cols: u64, rows: u64, data: &[u8]) -> Result<()> {
        let terminal = self.get(id).await?;
        let response = self
            .request(
                json!({
                    "op": "input",
                    "id": Self::wire_id(id)?,
                    "attachment_id": attachment_id,
                    "cols": cols,
                    "rows": rows,
                    "data_base64": wire::encode_base64(data),
                }),
                false,
            )
            .await?;
        Self::expect_ok(response)?;
        self.inner.current.lock().insert(terminal.session_id, id.to_string());
        Ok(())
    }

    /// `snapshot(id)`.
    pub async fn snapshot(&self, id: &str) -> Result<Snapshot> {
        self.get(id).await?;
        let response = self.request(json!({ "op": "snapshot", "id": Self::wire_id(id)? }), false).await?;
        let wire::WireResponse::Snapshot { process: terminal, text, checkpoint_base64, cursor_x, cursor_y } = response else {
            return Err(unexpected(&response));
        };
        Ok(Snapshot {
            info: Info::from_wire(&terminal),
            text,
            checkpoint: wire::decode_base64(&checkpoint_base64),
            cursor: Cursor { x: cursor_x, y: cursor_y },
        })
    }

    /// `read(sessionID, lines?)`: rows of the session's current (controller-selected) terminal.
    pub async fn read(&self, session_id: &str, lines: Option<i64>) -> Result<Option<ReadResult>> {
        if let Some(lines) = lines
            && !(1..=65535).contains(&lines) {
                return Err(Error::Unavailable("lines must be an integer between 1 and 65535".into()));
            }
        let Some(id) = self.inner.current.lock().get(session_id).cloned() else { return Ok(None) };
        let terminal = match self.get(&id).await {
            Ok(terminal) => Some(terminal),
            Err(Error::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        if terminal.is_none_or(|t| t.session_id != session_id) {
            let mut current = self.inner.current.lock();
            if current.get(session_id) == Some(&id) {
                current.remove(session_id);
            }
            return Ok(None);
        }
        // Let the daemon choose the live height in the same snapshot when lines is omitted.
        let mut request = json!({ "op": "read_rows", "id": Self::wire_id(&id)? });
        if let Some(lines) = lines {
            request["rows"] = lines.into();
        }
        let response = self.request(request, false).await?;
        let wire::WireResponse::Rows { process: terminal, lines, cursor_x, cursor_y } = response else {
            return Err(unexpected(&response));
        };
        let info = Info::from_wire(&terminal);
        Ok(Some(ReadResult {
            pty_id: info.id,
            title: info.title,
            cwd: info.cwd,
            foreground_process: info.foreground_process,
            screen: Screen {
                text: lines.join("\n"),
                cols: info.size.cols,
                rows: info.size.rows,
                cursor: Cursor { x: cursor_x, y: cursor_y },
            },
        }))
    }

    /// `remove(id)`: `terminate`; publishes `persistent-pty.removed`.
    pub async fn remove(&self, id: &str) -> Result<()> {
        let terminal = self.get(id).await?;
        let response = self.request(json!({ "op": "terminate", "id": Self::wire_id(id)? }), false).await?;
        Self::expect_ok(response)?;
        {
            let mut current = self.inner.current.lock();
            if current.get(&terminal.session_id).map(String::as_str) == Some(id) {
                current.remove(&terminal.session_id);
            }
        }
        self.publish(BusEvent::Removed { session_id: terminal.session_id, pty_id: id.to_string() });
        Ok(())
    }

    /// `rename(id, title)`: sets (or with `None` clears) the user title; publishes `persistent-pty.updated`.
    pub async fn rename(&self, id: &str, title: Option<&str>) -> Result<Info> {
        let terminal = self.get(id).await?;
        let response = self.request(json!({ "op": "rename", "id": Self::wire_id(id)?, "title": title }), false).await?;
        Self::expect_ok(response)?;
        let renamed = self.get(id).await?;
        self.publish(BusEvent::Updated { session_id: terminal.session_id, terminal: renamed.clone() });
        Ok(renamed)
    }

    /// `removeSession(sessionID)` (run on each `session.deleted`): removes the session's terminals. A terminal
    /// already gone is skipped and an unavailable daemon is only logged, so one failure never ends the cleanup.
    pub async fn remove_session(&self, session_id: &str) {
        match self.list(Some(session_id)).await {
            Ok(terminals) => {
                for terminal in terminals {
                    match self.remove(&terminal.id).await {
                        Ok(()) | Err(Error::NotFound(_)) => {}
                        Err(error) => tracing::warn!(
                            sessionID = %session_id,
                            id = %terminal.id,
                            error = %error,
                            "failed to remove deleted session's terminal"
                        ),
                    }
                }
            }
            Err(error) => tracing::warn!(sessionID = %session_id, error = %error, "failed to list deleted session's terminals"),
        }
        self.inner.current.lock().remove(session_id);
    }

    /// `shutdown()`: stop the daemon and all terminals (explicit restart/stop/uninstall).
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.handoff_expires_at.store(0, Ordering::SeqCst);
        let response = self.inner.daemon.shutdown().await?;
        self.inner.current.lock().clear();
        match response {
            None => Ok(()),
            Some(response) => Self::expect_ok(response),
        }
    }

    /// `handoff()`: prepare a restart handoff; `None` when no daemon is owned.
    pub async fn handoff(&self) -> Result<Option<Handoff>> {
        let prepared = self.inner.daemon.handoff().await?;
        if let Some(handoff) = &prepared {
            self.inner.handoff_expires_at.store(handoff.expires_at, Ordering::SeqCst);
        }
        Ok(prepared)
    }

    /// `handoffPrepared()`: whether a handoff to the next server is prepared and its ticket still valid. This
    /// server is being replaced, so the daemon's processes should be left running for the next one. A shutdown
    /// clears it.
    pub fn handoff_prepared(&self) -> bool {
        daemon::now_ms() < self.inner.handoff_expires_at.load(Ordering::SeqCst)
    }

    /// `createPipe(input)`: starts the daemon if needed. Fall back to another spawner only on
    /// [`PipeError::Unavailable`].
    pub async fn create_pipe(&self, input: CreatePipeInput) -> std::result::Result<PipeInfo, PipeError> {
        let response = self
            .inner
            .daemon
            .request(
                json!({
                    "op": "create_pipe",
                    "program": input.program,
                    "args": input.args,
                    "cwd": input.cwd,
                    "env": input.env,
                    "group_id": input.group_id,
                    "output_path": input.output_path,
                    "metadata": input.metadata,
                }),
                true,
            )
            .await
            .map_err(pipe_error)?;
        match &response {
            wire::WireResponse::Created { process } => PipeInfo::from_wire(process).ok_or_else(|| unexpected_pipe(&response)),
            _ => Err(unexpected_pipe(&response)),
        }
    }

    /// `waitPipe(id)`: resolves when the process exits, on its own connection with no timeout; dropping the future
    /// closes it.
    pub async fn wait_pipe(&self, id: u64) -> std::result::Result<PipeExit, PipeError> {
        self.inner.daemon.wait(id).await.map(PipeExit::from).map_err(pipe_error)
    }

    /// `getPipe(id)`: [`PipeError::NotFound`] for an unknown id, a terminal, or when no daemon is running.
    pub async fn get_pipe(&self, id: u64) -> std::result::Result<PipeInfo, PipeError> {
        let found = self.lookup(id).await.map_err(|error| PipeError::Unavailable(error.message))?;
        found.as_ref().and_then(PipeInfo::from_wire).ok_or(PipeError::NotFound(id))
    }

    async fn pipe_command(&self, value: Value) -> std::result::Result<(), PipeError> {
        let response = self.inner.daemon.request(value, false).await.map_err(pipe_error)?;
        if response == wire::WireResponse::Ok { Ok(()) } else { Err(unexpected_pipe(&response)) }
    }

    /// `signalPipe(id, signal)`: signals the process group without forgetting the process; `wait_pipe` then reports
    /// the signal.
    pub async fn signal_pipe(&self, id: u64, signal: Signal) -> std::result::Result<(), PipeError> {
        self.pipe_command(json!({ "op": "signal", "id": id, "signal": signal.as_str() })).await
    }

    /// `setPipeMetadata(id, metadata)`: replaces the stored metadata.
    pub async fn set_pipe_metadata(&self, id: u64, metadata: Metadata) -> std::result::Result<(), PipeError> {
        self.pipe_command(json!({ "op": "set_metadata", "id": id, "metadata": metadata })).await
    }

    /// `terminatePipe(id)`: kills the process if it still runs and forgets it; the output file is left alone.
    pub async fn terminate_pipe(&self, id: u64) -> std::result::Result<(), PipeError> {
        self.pipe_command(json!({ "op": "terminate", "id": id })).await
    }

    /// `listPipes()`: never starts the daemon; empty when none is running.
    pub async fn list_pipes(&self) -> std::result::Result<Vec<PipeInfo>, PipeError> {
        let response =
            self.inner.daemon.request_if_running(json!({ "op": "list", "mode": "pipe" })).await.map_err(pipe_error)?;
        match response {
            None => Ok(Vec::new()),
            Some(wire::WireResponse::Processes { processes }) => Ok(processes.iter().filter_map(PipeInfo::from_wire).collect()),
            Some(other) => Err(unexpected_pipe(&other)),
        }
    }

    /// `attach(id, input)`: subscribe with replay from `cursor`. Call [`Attachment::activate`]
    /// after sending the replay to start live events. A controller attachment selects the
    /// terminal for `read`. An observed `exited` event removes the terminal in the background.
    pub async fn attach(&self, id: &str, input: AttachInput) -> Result<Attachment> {
        self.get(id).await?;
        let attachment = self
            .inner
            .daemon
            .subscribe(
                Self::wire_id(id)?,
                &SubscribeInput {
                    cursor: input.cursor,
                    attachment_id: input.attachment_id,
                    role: input.role,
                    takeover: input.takeover,
                },
            )
            .await?;
        let info = Info::from_wire(&attachment.terminal);
        if attachment.role == Role::Controller {
            self.inner.current.lock().insert(info.session_id.clone(), id.to_string());
        }
        let weak = Arc::downgrade(&self.inner);
        let pty_id = id.to_string();
        let hook: daemon::EventHook = Arc::new(move |event: &StreamEvent| {
            if matches!(event, StreamEvent::Exited { .. }) {
                remove_visible_exit(&weak, &pty_id);
            }
        });
        Ok(Attachment {
            info,
            role: attachment.role,
            generation: attachment.generation,
            replay: attachment.replay.clone(),
            inner: attachment,
            hook,
        })
    }
}

/// `parseID`: the daemon id of one of our terminal IDs.
fn parse_id(value: &str) -> Option<u64> {
    from_id(value).ok()
}

/// `removeVisibleExit`: background, de-duplicated removal of a terminal seen exiting.
fn remove_visible_exit(weak: &Weak<Inner>, id: &str) {
    let Some(inner) = weak.upgrade() else { return };
    if !inner.removing.lock().insert(id.to_string()) {
        return;
    }
    let id = id.to_string();
    tokio::spawn(async move {
        let service = PersistentPty { inner };
        match service.remove(&id).await {
            Ok(()) | Err(Error::NotFound(_)) => {}
            Err(error) => {
                tracing::warn!(id = %id, error = %error, "failed to remove visible exited terminal");
            }
        }
        service.inner.removing.lock().remove(&id);
    });
}

/// `binary ??= resolveBinary(global.bin).catch(reset)`.
fn memoized_binary(resolver: BinaryResolver) -> BinaryFn {
    let cache: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));
    Arc::new(move || {
        let cache = cache.clone();
        let resolver = resolver.clone();
        Box::pin(async move {
            let mut cached = cache.lock().await;
            if let Some(path) = cached.as_ref() {
                return Ok(path.clone());
            }
            let path = resolver.resolve().await?;
            *cached = Some(path.clone());
            Ok(path)
        })
    })
}

/// `ShellSelect.environment(bin)`: `$SHELL` if it resolves to an existing file (absolute) or is
/// found on `PATH` (+ `bin`); otherwise `/bin/zsh` on macOS, `bash` on `PATH`, else `/bin/sh`.
/// On Windows: first of pwsh, powershell, `$COMSPEC || cmd.exe`.
pub fn shell_environment(bin: &Path) -> String {
    let find = |name: &str| -> Option<String> {
        let mut paths: Vec<PathBuf> =
            std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        paths.push(bin.to_path_buf());
        let joined = std::env::join_paths(paths).ok()?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        which::which_in(name, Some(joined), cwd).ok().map(|p| p.to_string_lossy().into_owned())
    };
    if let Some(shell) = std::env::var("SHELL").ok().filter(|s| !s.is_empty()) {
        if Path::new(&shell).is_absolute() {
            if std::fs::metadata(&shell).is_ok_and(|m| m.is_file()) {
                return shell;
            }
        } else if let Some(found) = find(&shell) {
            return found;
        }
    }
    if cfg!(windows) {
        return find("pwsh")
            .or_else(|| find("powershell"))
            .unwrap_or_else(|| std::env::var("COMSPEC").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "cmd.exe".into()));
    }
    if cfg!(target_os = "macos") {
        return "/bin/zsh".into();
    }
    find("bash").unwrap_or_else(|| "/bin/sh".into())
}
