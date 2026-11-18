//! Non-interactive shell command processes (`core/src/shell.ts`).
//!
//! Each command spawns once, streams combined stdout+stderr into
//! `<data>/shell/<projectID>/<id>.out`, and keeps the last 25 exited commands observable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use oc_schema::Metadata;
use oc_schema::message::{ShellInfo, ShellOutput, ShellStatus, ShellTime};
use parking_lot::Mutex;
use tokio::sync::watch;

use super::result::{ShellResult, Written, capture_start, capture_tail};

/// `Signal number → name`, as a process killed by it reports it.
pub fn signal_name(signal: i32) -> String {
    super::result::signal_name(signal)
}
use crate::host::ShellInvocation;

pub const EXITED_LIMIT: usize = 25;
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
pub const DIRECTORY: &str = "shell";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotFound {
    pub id: String,
}

/// Bus events (`Shell.Event.*`).
#[derive(Debug, Clone, PartialEq)]
pub enum ShellEvent {
    Created { info: ShellInfo },
    Exited { id: String, exit: Option<i64>, status: ShellStatus },
    Deleted { id: String },
}

pub type EventSink = Arc<dyn Fn(ShellEvent) + Send + Sync>;

type Done = Option<Result<ShellInfo, NotFound>>;

struct Active {
    info: Mutex<ShellInfo>,
    file: PathBuf,
    size: AtomicU64,
    newlines: AtomicU64,
    /// Byte offsets where the last two lines start, so a truncation notice can size a cut-off line. Updated with
    /// `size` and `newlines` while the output file's lock is held.
    line_starts: Mutex<[u64; 2]>,
    done: watch::Sender<Done>,
    deadline: watch::Sender<Option<tokio::time::Instant>>,
    /// A recorded stop (`Shell.stop`): the status the command ends with once it exits. Wakes the manager to kill it.
    stop: watch::Sender<Option<ShellStatus>>,
    /// How the command ended and when (`ended`): the process exit, the timeout and a stop race to record it, and the
    /// first wins. A stop is recorded when it is asked for, so the command reports it even if the process exits (or
    /// its exit is observed) before the kill.
    ended: Mutex<Option<(ShellStatus, u64)>>,
    pid: Option<u32>,
    /// The Job Object holding the command and everything it starts, which stops them all at once.
    #[cfg(windows)]
    job: Option<windows_job::Job>,
}

pub struct ShellService {
    data: PathBuf,
    commands: Mutex<HashMap<String, Arc<Active>>>,
    exit_order: Mutex<Vec<String>>,
    events: Option<EventSink>,
}

impl Active {
    /// Records how the command ended, unless something already did; returns whether this did (`end`).
    fn end(&self, status: ShellStatus) -> bool {
        let mut ended = self.ended.lock();
        if ended.is_some() {
            return false;
        }
        *ended = Some((status, crate::util::now_ms()));
        true
    }
}

#[cfg(unix)]
fn kill_group(pid: u32, signal: i32) {
    // SAFETY: plain libc call with a process-group id we spawned.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}


/// Stops the command and what it started: SIGTERM to its process group, then SIGKILL after 3 s. Windows has no
/// signal to ask with, so the whole tree ends at once, as `taskkill /T /F` ends it upstream: through the command's
/// Job Object, which needs no helper process and also holds what the command was starting meanwhile, or with
/// `taskkill` when the command has none.
fn kill_tree(active: &Active) {
    #[cfg(windows)]
    if active.job.as_ref().is_some_and(|job| job.terminate()) {
        return;
    }
    let Some(pid) = active.pid else { return };
    #[cfg(unix)]
    {
        kill_group(pid, libc::SIGTERM);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            kill_group(pid, libc::SIGKILL);
        });
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill").args(["/pid", &pid.to_string(), "/T", "/F"]).output();
    }
}

#[cfg(windows)]
mod windows_job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject};

    /// A Job Object holding one command's processes; processes they start join it too.
    pub struct Job(HANDLE);

    // SAFETY: a Job Object handle can be used and closed from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// A new Job Object holding `process` (a handle with PROCESS_SET_QUOTA and PROCESS_TERMINATE access, as
        /// the spawner's handle has).
        pub fn assign(process: HANDLE) -> Option<Job> {
            // SAFETY: plain Win32 calls; the job handle is owned by the returned `Job` (or closed here).
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                if AssignProcessToJobObject(job, process) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        }

        /// Ends every process in the job, with exit code 1 as `taskkill /F` gives them.
        pub fn terminate(&self) -> bool {
            // SAFETY: the handle is valid until drop.
            unsafe { TerminateJobObject(self.0, 1) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: owned handle, closed once. Closing it leaves the processes running.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

impl ShellService {
    /// `data` = global data dir.
    pub fn new(data: PathBuf, events: Option<EventSink>) -> Arc<Self> {
        Arc::new(Self { data, commands: Mutex::new(HashMap::new()), exit_order: Mutex::new(Vec::new()), events })
    }

    pub fn output_dir(&self, project_id: &str) -> PathBuf {
        self.data.join(DIRECTORY).join(project_id)
    }

    fn emit(&self, event: ShellEvent) {
        if let Some(sink) = &self.events {
            sink(event);
        }
    }

    fn require(&self, id: &str) -> Result<Arc<Active>, NotFound> {
        self.commands.lock().get(id).cloned().ok_or_else(|| NotFound { id: id.to_string() })
    }

    fn remove_command(&self, id: &str) {
        self.exit_order.lock().retain(|x| x != id);
        let Some(command) = self.commands.lock().remove(id) else { return };
        let _ = command.deadline.send(None);
        let running = command.info.lock().status == ShellStatus::Running;
        if running {
            kill_tree(&command);
        }
        // Every shell reports exactly one exit; removing a running one ends it as killed (ce15cd882).
        if running {
            self.emit(ShellEvent::Exited { id: id.to_string(), exit: None, status: ShellStatus::Killed });
        }
        command.done.send_if_modified(|done| {
            if done.is_none() {
                *done = Some(Err(NotFound { id: id.to_string() }));
                true
            } else {
                false
            }
        });
        let _ = std::fs::remove_file(&command.file);
        self.emit(ShellEvent::Deleted { id: id.to_string() });
    }

    pub fn remove(&self, id: &str) -> Result<(), NotFound> {
        self.require(id)?;
        self.remove_command(id);
        Ok(())
    }

    /// Running commands only.
    pub fn list(&self) -> Vec<ShellInfo> {
        self.commands.lock().values().map(|c| c.info.lock().clone()).filter(|i| i.status == ShellStatus::Running).collect()
    }

    pub fn get(&self, id: &str) -> Result<ShellInfo, NotFound> {
        Ok(self.require(id)?.info.lock().clone())
    }

    pub async fn wait(&self, id: &str) -> Result<ShellInfo, NotFound> {
        let command = self.require(id)?;
        let mut rx = command.done.subscribe();
        loop {
            if let Some(done) = rx.borrow().clone() {
                return done;
            }
            if rx.changed().await.is_err() {
                return Err(NotFound { id: id.to_string() });
            }
        }
    }

    /// Replace the running command's timeout from now; zero clears it.
    pub fn timeout(&self, id: &str, duration_ms: u64) -> Result<ShellInfo, NotFound> {
        let command = self.require(id)?;
        let info = command.info.lock().clone();
        if info.status != ShellStatus::Running {
            return Ok(info);
        }
        let _ = command
            .deadline
            .send((duration_ms > 0).then(|| tokio::time::Instant::now() + Duration::from_millis(duration_ms)));
        Ok(info)
    }

    /// Stops a running command gracefully (SIGTERM, then SIGKILL after 3 s) without forgetting it: it ends with the
    /// given status (`Killed`, or `Timeout`) and stays observable, file included, until retention evicts it. Returns
    /// at once; [`ShellService::wait`] observes the exit. Ignored once the command has ended or is already stopping
    /// (`Shell.stop`, 2.3.0).
    pub fn stop(&self, id: &str, reason: ShellStatus) -> Result<ShellInfo, NotFound> {
        let command = self.require(id)?;
        let info = command.info.lock().clone();
        if info.status != ShellStatus::Running {
            return Ok(info);
        }
        if !command.end(reason) {
            return Ok(info);
        }
        command.stop.send_if_modified(|stop| {
            if stop.is_some() {
                return false;
            }
            *stop = Some(reason);
            true
        });
        Ok(info)
    }

    /// Output page by byte cursor. A page never splits a UTF-8 character: it starts after the rest of one the cursor
    /// cut, and ends before one the limit would cut, holding back a valid prefix at the end of a running command's
    /// output (#63984).
    pub fn output(&self, id: &str, cursor: Option<u64>, limit: Option<u64>) -> Result<ShellOutput, NotFound> {
        let command = self.require(id)?;
        let running = command.info.lock().status == ShellStatus::Running;
        let size = command.size.load(Ordering::SeqCst);
        let cursor = cursor.unwrap_or(0);
        let limit = limit.unwrap_or(65536);
        if cursor >= size {
            return Ok(ShellOutput { output: String::new(), cursor: size, size, truncated: false });
        }
        let start = cursor;
        if limit == 0 {
            return Ok(ShellOutput { output: String::new(), cursor: start, size, truncated: false });
        }
        // A UTF-8 character can straddle either edge by up to three bytes.
        let offset = start.saturating_sub(3);
        let bytes = read_range(&command.file, offset, size.min(start.saturating_add(limit).saturating_add(3)) - offset).unwrap_or_default();
        let (output, cursor) = output_page(&bytes, offset, start, limit, running);
        Ok(ShellOutput { output, cursor, size, truncated: false })
    }

    /// Terminal state and bounded tail.
    pub async fn result(&self, started: &ShellInfo, max_lines: usize, max_bytes: usize) -> ShellResult {
        self.result_within(started, None, max_lines, max_bytes).await
    }

    /// [`ShellService::result`]; with `wait`, stops waiting for the exit after that long and returns the output so
    /// far, still running.
    pub async fn result_within(&self, started: &ShellInfo, wait: Option<Duration>, max_lines: usize, max_bytes: usize) -> ShellResult {
        let ended = match wait {
            None => Some(self.wait(&started.id).await),
            Some(limit) => tokio::time::timeout(limit, self.wait(&started.id)).await.ok(),
        };
        let ended = match ended {
            Some(ended) => ended,
            None => self.get(&started.id),
        };
        let info = match ended {
            Ok(info) => info,
            Err(_) => {
                let mut info = started.clone();
                info.status = ShellStatus::Killed;
                info.time.completed = Some(crate::util::now_ms());
                info
            }
        };
        let capture = self.require(&info.id).ok().map(|command| {
            let written = Written {
                size: command.size.load(Ordering::SeqCst),
                newlines: command.newlines.load(Ordering::SeqCst),
                line_starts: *command.line_starts.lock(),
            };
            // Read the byte before the window too, to tell whether the window starts on a line boundary.
            let before = capture_start(written.size, max_bytes).saturating_sub(1);
            let window = read_range(&command.file, before, written.size - before).unwrap_or_default();
            capture_tail(&window, written, max_lines, max_bytes, &info.file)
        });
        ShellResult { info, capture }
    }

    /// Spawn a prepared invocation. Returns once the process is registered.
    pub async fn spawn(self: &Arc<Self>, invocation: &ShellInvocation, metadata: Metadata, project_id: &str) -> Result<ShellInfo, String> {
        let id = oc_schema::id::shell();
        let dir = self.output_dir(project_id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join(format!("{id}.out"));
        let args = super::select::args(&invocation.shell, &invocation.command);
        let mut command = tokio::process::Command::new(&invocation.shell);
        command
            .args(&args)
            .current_dir(&invocation.cwd)
            .env_clear()
            .envs(&invocation.env)
            .stdin(Stdio::null());
        // stdout and stderr share one pipe, so the output keeps the order the command wrote it in (two pipes read
        // by two tasks could swap chunks written back to back, like `echo out; echo err >&2`).
        let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
        let stderr_writer = writer.try_clone().map_err(|e| e.to_string())?;
        command.stdout(Stdio::from(writer)).stderr(Stdio::from(stderr_writer));
        #[cfg(unix)]
        command.process_group(0);
        let spawned = command.spawn();
        // Close this process's copies of the write end, so the reader sees the end once the command (and anything
        // it left running with the pipe) is gone.
        drop(command);
        let mut child = spawned.map_err(|e| e.to_string())?;
        let pid = child.id();
        #[cfg(windows)]
        let job = child.raw_handle().and_then(|process| windows_job::Job::assign(process as _));
        let info = ShellInfo {
            id: id.clone(),
            status: ShellStatus::Running,
            command: invocation.command.clone(),
            cwd: invocation.cwd.clone(),
            shell: invocation.shell.clone(),
            file: file.to_string_lossy().into_owned(),
            pid,
            exit: None,
            signal: None,
            metadata,
            time: ShellTime { started: crate::util::now_ms(), completed: None },
        };
        let (done, _) = watch::channel(None);
        let (deadline, mut deadline_rx) = watch::channel(None);
        let (stop, mut stop_rx) = watch::channel(None);
        let active = Arc::new(Active {
            info: Mutex::new(info.clone()),
            file: file.clone(),
            size: AtomicU64::new(0),
            newlines: AtomicU64::new(0),
            line_starts: Mutex::new([0, 0]),
            done,
            deadline,
            stop,
            ended: Mutex::new(None),
            pid,
            #[cfg(windows)]
            job,
        });
        self.commands.lock().insert(id.clone(), active.clone());
        if invocation.timeout > 0 {
            let _ = active.deadline.send(Some(tokio::time::Instant::now() + Duration::from_millis(invocation.timeout)));
        }

        let out = std::sync::Arc::new(Mutex::new(std::fs::File::create(&file).map_err(|e| e.to_string())?));
        let output_task = {
            let active = active.clone();
            let out = out.clone();
            tokio::task::spawn_blocking(move || {
                use std::io::{Read, Write};
                let mut reader = reader;
                let mut buffer = vec![0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = &buffer[..n];
                            let mut file = out.lock();
                            let _ = file.write_all(chunk);
                            let size = active.size.load(Ordering::SeqCst);
                            let mut line_starts = active.line_starts.lock();
                            for (index, _) in chunk.iter().enumerate().filter(|(_, b)| **b == b'\n') {
                                active.newlines.fetch_add(1, Ordering::SeqCst);
                                *line_starts = [line_starts[1], size + index as u64 + 1];
                            }
                            active.size.store(size + n as u64, Ordering::SeqCst);
                        }
                    }
                }
                let _ = out.lock().flush();
            })
        };

        let service = Arc::downgrade(self);
        let manager_active = active.clone();
        tokio::spawn(async move {
            let active = manager_active;
            // The process exit, the timeout and a stop race to record how the command ended (`Active::end`); the
            // first wins. A stop (a timeout is one) keeps its status but still records how the killed process exited.
            // The terminal status is applied only once captured output is complete (3bdab634e).
            let status = loop {
                let deadline = *deadline_rx.borrow();
                tokio::select! {
                    status = child.wait() => break status.ok(),
                    _ = async {
                        match deadline {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        // Timeout: record it (unless a stop already did), kill the process group, keep waiting for
                        // exit.
                        active.end(ShellStatus::Timeout);
                        kill_tree(&active);
                        let _ = active.deadline.send(None);
                        let status = child.wait().await.ok();
                        break status;
                    }
                    changed = stop_rx.changed() => {
                        let reason = *stop_rx.borrow();
                        // `stop` recorded it; a recorded stop always kills.
                        if let (Ok(()), Some(_)) = (changed, reason) {
                            kill_tree(&active);
                            let _ = active.deadline.send(None);
                            break child.wait().await.ok();
                        }
                    }
                    changed = deadline_rx.changed() => {
                        if changed.is_err() {
                            break child.wait().await.ok();
                        }
                    }
                }
            };
            active.end(ShellStatus::Exited);
            let _ = output_task.await;
            let snapshot = {
                let mut info = active.info.lock();
                if info.status == ShellStatus::Running {
                    // A timeout keeps its status and time but still records how the killed process exited.
                    let (ended, completed) = active.ended.lock().unwrap_or((ShellStatus::Exited, crate::util::now_ms()));
                    (info.status, info.time.completed) = (ended, Some(completed));
                    if let Some(status) = status {
                        match status.code() {
                            Some(code) => info.exit = Some(code as i64),
                            None => {
                                #[cfg(unix)]
                                {
                                    use std::os::unix::process::ExitStatusExt;
                                    info.signal = status.signal().map(signal_name);
                                }
                            }
                        }
                    }
                }
                info.clone()
            };
            if snapshot.status == ShellStatus::Running {
                return;
            }
            active.done.send_if_modified(|done| {
                if done.is_none() {
                    *done = Some(Ok(snapshot.clone()));
                    true
                } else {
                    false
                }
            });
            let Some(service) = service.upgrade() else { return };
            if !service.commands.lock().contains_key(&snapshot.id) {
                return;
            }
            service.emit(ShellEvent::Exited { id: snapshot.id.clone(), exit: snapshot.exit, status: snapshot.status });
            let evict: Vec<String> = {
                let mut order = service.exit_order.lock();
                order.push(snapshot.id.clone());
                let excess = order.len().saturating_sub(EXITED_LIMIT);
                order.iter().take(excess).cloned().collect()
            };
            for old in evict {
                service.remove_command(&old);
            }
        });
        self.emit(ShellEvent::Created { info: info.clone() });
        Ok(info)
    }

    /// Hourly sweep of `<data>/shell/*/sh_*.out` files older than 7 days.
    pub fn cleanup(data: &Path) {
        let directory = data.join(DIRECTORY);
        let re = regex::Regex::new(r"^sh_[0-9a-f]{12}.*\.out$").unwrap();
        let mut files = Vec::new();
        if let Ok(projects) = std::fs::read_dir(&directory) {
            for project in projects.flatten().filter(|p| p.file_type().map(|t| t.is_dir()).unwrap_or(false)) {
                if let Ok(entries) = std::fs::read_dir(project.path()) {
                    for entry in entries.flatten() {
                        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) && re.is_match(&entry.file_name().to_string_lossy()) {
                            files.push(entry.path());
                        }
                    }
                }
            }
        }
        crate::tool_output::retention_cleanup(&files, RETENTION);
    }
}

/// One page of output from `bytes`, the file's bytes from `offset` (up to three bytes before the cursor `start`) to at
/// most three bytes past `start + limit`: the decoded page and the cursor after it.
fn output_page(bytes: &[u8], offset: u64, start: u64, limit: u64, running: bool) -> (String, u64) {
    let from = ((start - offset) as usize).min(bytes.len());
    let to = bytes.len().min(from.saturating_add(limit.min(usize::MAX as u64) as usize));
    let first = split_character(bytes, from, running).and_then(|split| split.end).unwrap_or(from);
    let last = split_character(bytes, to, running);
    // Keep the usual limit unless even the first complete character would not fit.
    let end = match last {
        Some(Split { start, end: Some(end) }) if start == first => end,
        Some(split) => split.start,
        None => to,
    };
    let output = if end > first { String::from_utf8_lossy(&bytes[first..end]).into_owned() } else { String::new() };
    (output, offset + first.max(end) as u64)
}

/// A UTF-8 character crossing a byte offset: where it starts, and where it ends (none for a valid prefix at a live
/// end of output).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Split {
    start: usize,
    end: Option<usize>,
}

/// `splitCharacter`: the character crossing `cut`, if a well-formed one does.
fn split_character(bytes: &[u8], cut: usize, running: bool) -> Option<Split> {
    for start in (cut.saturating_sub(3)..cut).rev() {
        let lead = bytes[start];
        if lead & 0xc0 == 0x80 {
            continue;
        }
        if !(0xc2..=0xf4).contains(&lead) {
            return None;
        }
        let end = start + if lead < 0xe0 { 2 } else if lead < 0xf0 { 3 } else { 4 };
        if end <= cut {
            return None;
        }
        if end <= bytes.len() {
            return std::str::from_utf8(&bytes[start..end]).is_ok().then_some(Split { start, end: Some(end) });
        }
        // A valid prefix of a character: what a streaming decoder holds back without a replacement.
        let prefix = &bytes[start..];
        let valid = match std::str::from_utf8(prefix) {
            Ok(_) => true,
            Err(error) => error.valid_up_to() == 0 && error.error_len().is_none(),
        };
        return (running && valid).then_some(Split { start, end: None });
    }
    None
}

fn read_range(path: &Path, start: u64, length: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// shell-daemon.test.ts (2.3.1, #63984): pages keep UTF-8 characters whole on both edges.
#[cfg(test)]
mod page_tests {
    use super::output_page;

    /// `shell.output(id, {cursor, limit})` over a file holding `file`.
    fn page(file: &[u8], cursor: u64, limit: u64, running: bool) -> (String, u64) {
        let size = file.len() as u64;
        if cursor >= size {
            return (String::new(), size);
        }
        if limit == 0 {
            return (String::new(), cursor);
        }
        let offset = cursor.saturating_sub(3);
        let end = size.min(cursor + limit + 3);
        output_page(&file[offset as usize..end as usize], offset, cursor, limit, running)
    }

    #[test]
    fn pages_at_utf8_boundaries_with_byte_cursors() {
        let prefix = "a".repeat(65535);
        for c in ["é", "€", "😀"] {
            let text = format!("{prefix}{c}\u{FEFF}z");
            let bytes = text.as_bytes();
            let (first, cursor) = page(bytes, 0, 65536, true);
            assert_eq!(cursor, prefix.len() as u64);
            let (rest, end) = page(bytes, cursor, 65536, true);
            assert_eq!(format!("{first}{rest}"), text);
            assert_eq!(end, bytes.len() as u64);
            assert_eq!(page(bytes, cursor, 1, true), (c.to_string(), cursor + c.len() as u64));
            assert_eq!(page(bytes, cursor + 1, 65536, true), ("\u{FEFF}z".to_string(), bytes.len() as u64));
        }
        assert_eq!(page(&[b'a'; 65537], 65536, 0, true), (String::new(), 65536));
    }

    #[test]
    fn retries_an_incomplete_live_character_and_consumes_malformed_bytes_at_exit() {
        let start = [0xff, 0xf0, 0x9f];
        assert_eq!(page(&start, 0, 65536, true), ("\u{FFFD}".to_string(), 1));
        assert_eq!(page(&start, 1, 1, true), (String::new(), 1));
        let more = [0xff, 0xf0, 0x9f, 0x98, 0x80, 0xe0, 0x80, 0xe2];
        assert_eq!(page(&more, 1, 65536, true), ("😀\u{FFFD}\u{FFFD}".to_string(), 7));
        assert_eq!(page(&more, 7, 65536, false), ("\u{FFFD}".to_string(), 8));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn invocation(command: &str, cwd: &Path, timeout: u64) -> ShellInvocation {
        ShellInvocation {
            command: command.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            timeout,
            shell: "/bin/sh".into(),
            env: std::env::vars().collect(),
        }
    }

    #[tokio::test]
    async fn runs_and_captures() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let info = service.spawn(&invocation("echo hi; echo err 1>&2; exit 3", dir.path(), 0), Metadata::new(), "proj").await.unwrap();
        assert!(info.id.starts_with("sh_"));
        let result = service.result(&info, 2000, 51200).await;
        assert_eq!(result.info.status, ShellStatus::Exited);
        assert_eq!(result.info.exit, Some(3));
        let capture = result.capture.unwrap();
        assert_eq!(capture.output, "hi\nerr\n");
        assert!(!capture.truncated);
    }

    /// stdout and stderr written back to back keep their order (they share one pipe).
    #[tokio::test]
    async fn interleaved_stdout_and_stderr_keep_their_order() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let script = "i=0; while [ $i -lt 300 ]; do echo out$i; echo err$i 1>&2; i=$((i+1)); done";
        let info = service.spawn(&invocation(script, dir.path(), 0), Metadata::new(), "p").await.unwrap();
        let result = service.result(&info, 5000, 1 << 20).await;
        let expected: String = (0..300).map(|i| format!("out{i}\nerr{i}\n")).collect();
        assert_eq!(result.capture.unwrap().output, expected);
    }

    #[tokio::test]
    async fn times_out() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let info = service.spawn(&invocation("echo start; sleep 5", dir.path(), 200), Metadata::new(), "p").await.unwrap();
        let result = service.result(&info, 2000, 51200).await;
        assert_eq!(result.info.status, ShellStatus::Timeout);
        assert_eq!(result.capture.unwrap().output, "start\n");
        // #62141: a timeout still records how the killed process exited.
        assert_eq!((result.info.exit, result.info.signal.as_deref()), (None, Some("SIGTERM")));
    }

    /// tool-shell.test.ts truncation cases (#62141) through the real output file, stdout and stderr together.
    #[tokio::test]
    async fn truncated_capture_counts_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let lines: Vec<String> = (1..=10).map(|i| format!("{i}{}", "x".repeat(10 * 1024 - i.to_string().len()))).collect();
        std::fs::write(dir.path().join("long.txt"), lines.join("\n") + "\n").unwrap();
        std::fs::write(dir.path().join("line.txt"), format!("head\n{}", "x".repeat(200 * 1024))).unwrap();
        let info = service.spawn(&invocation("cat long.txt", dir.path(), 0), Metadata::new(), "p").await.unwrap();
        let capture = service.result(&info, 2000, 51200).await.capture.unwrap();
        assert!(capture.output.starts_with("7xxx"));
        assert!(capture.output.contains(&format!("{}\n\n[showing lines 7-10 of 10; full output saved to ", lines[9])));
        let info = service.spawn(&invocation("cat line.txt 1>&2", dir.path(), 0), Metadata::new(), "p").await.unwrap();
        let capture = service.result(&info, 2000, 51200).await.capture.unwrap();
        assert!(capture.output.starts_with(&format!("{}\n\n", "x".repeat(51200))));
        assert!(capture.output.contains("[showing the last 50 KB of line 2 (200 KB); full output saved to "));
    }

    /// shell.test.ts "passes an unmatched zsh glob through to the command" (#62141).
    #[tokio::test]
    async fn zsh_passes_unmatched_globs_through() {
        let Some(zsh) = crate::util::which("zsh", None) else { return };
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let mut run = invocation("echo opencode-missing-*.nope; echo after", dir.path(), 0);
        run.shell = zsh.to_string_lossy().into_owned();
        let info = service.spawn(&run, Metadata::new(), "p").await.unwrap();
        let result = service.result(&info, 2000, 51200).await;
        assert_eq!((result.capture.unwrap().output.as_str(), result.info.exit), ("opencode-missing-*.nope\nafter\n", Some(0)));
    }

    #[tokio::test]
    async fn removal_reports_killed() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let info = service.spawn(&invocation("sleep 5", dir.path(), 0), Metadata::new(), "p").await.unwrap();
        let s2 = service.clone();
        let id = info.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            s2.remove(&id).unwrap();
        });
        let result = service.result(&info, 2000, 51200).await;
        assert_eq!(result.info.status, ShellStatus::Killed);
        assert!(result.capture.is_none());
    }

    // ce15cd882: removing a running command publishes its exit as killed before the deletion; an exited one
    // only publishes the deletion.
    #[tokio::test]
    async fn removal_publishes_killed_before_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::default();
        let sink = events.clone();
        let service = ShellService::new(dir.path().to_path_buf(), Some(Arc::new(move |e| sink.lock().push(e))));
        let running = service.spawn(&invocation("sleep 5", dir.path(), 0), Metadata::new(), "p").await.unwrap();
        service.remove(&running.id).unwrap();
        let done = service.spawn(&invocation("true", dir.path(), 0), Metadata::new(), "p").await.unwrap();
        service.wait(&done.id).await.unwrap();
        service.remove(&done.id).unwrap();
        let kinds: Vec<String> = events
            .lock()
            .iter()
            .map(|e| match e {
                ShellEvent::Created { info } => format!("created {}", info.id == running.id),
                ShellEvent::Exited { id, status, .. } => format!("exited {} {status:?}", *id == running.id),
                ShellEvent::Deleted { id } => format!("deleted {}", *id == running.id),
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["created true", "exited true Killed", "deleted true", "created false", "exited false Exited", "deleted false"]
        );
    }

    // 3bdab634e: a timed-out command reports its terminal status (and keeps the time it timed out) only once
    // output capture is complete, and retiming it after the end is ignored.
    #[tokio::test]
    async fn timeout_is_recorded_when_the_command_ends() {
        let dir = tempfile::tempdir().unwrap();
        let service = ShellService::new(dir.path().to_path_buf(), None);
        let info = service.spawn(&invocation("echo start; sleep 5", dir.path(), 100), Metadata::new(), "p").await.unwrap();
        let ended = service.wait(&info.id).await.unwrap();
        assert_eq!(ended.status, ShellStatus::Timeout);
        assert!(ended.time.completed.is_some());
        assert_eq!(service.timeout(&info.id, 60_000).unwrap().status, ShellStatus::Timeout);
        assert_eq!(service.get(&info.id).unwrap(), ended);
    }
}
