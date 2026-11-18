//! An in-process `opencode-pty` speaking protocol 9 over a Unix socket, for tests (feature `testing`). A port of
//! upstream's `test/fixture/persistent-pty-daemon.ts`: pipe processes run nothing; they stay running until
//! [`FakeDaemon::exit`] or a signal ends them (SIGTERM spares those in `trap`), and write only what
//! [`FakeDaemon::output`] gives them. A server adopts the fake through [`FakeDaemon::handoff`] (or by naming its
//! directory), so no daemon is spawned.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, oneshot};

use crate::frame::{FrameReader, encode, encode_payload, output_frame_payload};

/// The pid the fake registers and answers pings with.
pub const PID: u64 = 4242;

/// One process the fake knows (`ProcessInfo` as JSON plus its bookkeeping).
#[derive(Debug, Clone)]
pub struct Process {
    pub info: Value,
}

impl Process {
    pub fn id(&self) -> u64 {
        self.info["id"].as_u64().unwrap_or(0)
    }
    pub fn running(&self) -> bool {
        self.info["lifecycle"]["status"] == "running"
    }
}

#[derive(Default)]
pub struct State {
    pub token: String,
    pub protocol: u64,
    pub instance: String,
    /// Answer pings with another pid (a registration mismatch).
    pub pong_pid: Option<u64>,
    pub processes: Vec<Process>,
    pub next_id: u64,
    /// Every request after authentication, in order (including those on the owner connection).
    pub requests: Vec<Value>,
    /// `create_pipe` fails with this message.
    pub fail_create: Option<String>,
    /// Pipe ids a SIGTERM does not end.
    pub trap: HashSet<u64>,
    /// How long a prepared handoff's ticket stays valid (ms).
    pub handoff_ttl: u64,
    /// The ticket `prepare_handoff` returns.
    pub ticket: String,
    /// Ops whose connection closes without a reply (after the request was recorded).
    pub hang_up: HashSet<String>,
    waiters: HashMap<u64, Vec<oneshot::Sender<Option<Value>>>>,
}

pub struct FakeDaemon {
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub state: Mutex<State>,
    /// Fail the next N non-ping requests with "authentication failed" (and rotate the token).
    pub auth_failures: AtomicUsize,
    /// Closes the owner connection.
    pub owner_close: Notify,
    closed: std::sync::atomic::AtomicBool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A terminal as the fake lists it.
pub fn terminal(id: u64, group: &str) -> Value {
    json!({
        "id": id, "mode": "pty", "pid": 99, "command": ["/bin/sh"], "cwd": "/", "group_id": group,
        "lifecycle": {"status": "running"}, "output_head": 0, "output_tail": 5, "started_at": now_ms(),
        "exited_at": null,
        "pty": {"title": "Terminal", "user_title": null, "foreground_process": null, "cols": 80, "rows": 24}
    })
}

fn pipe(id: u64, request: &Value) -> Value {
    let mut command = vec![request["program"].clone()];
    command.extend(request["args"].as_array().cloned().unwrap_or_default());
    json!({
        "id": id, "mode": "pipe", "pid": 1000 + id, "command": command, "cwd": request["cwd"],
        "group_id": request["group_id"], "lifecycle": {"status": "running"}, "output_head": 0, "output_tail": 0,
        "started_at": now_ms(), "exited_at": null,
        "pipe": {"output_path": request["output_path"], "output_lines": 0,
                 "metadata": if request["metadata"].is_object() { request["metadata"].clone() } else { json!({}) }}
    })
}

fn exited(info: &Value) -> Value {
    let lifecycle = &info["lifecycle"];
    let mut reply = json!({
        "type": "exited",
        "exit_code": lifecycle.get("exit_code").cloned().unwrap_or(Value::Null),
        "final_offset": info["output_tail"],
        "exited_at": info["exited_at"].as_u64().unwrap_or_else(now_ms),
    });
    if let Some(signal) = lifecycle.get("signal").filter(|s| !s.is_null()) {
        reply["signal"] = signal.clone();
    }
    if let Some(pipe) = info.get("pipe") {
        reply["output_lines"] = pipe["output_lines"].clone();
        if let Some(error) = pipe.get("output_error").filter(|e| !e.is_null()) {
            reply["output_error"] = error.clone();
        }
    }
    reply
}

impl FakeDaemon {
    /// Starts a fake registered in `directory` (`service.json`), with no processes.
    pub async fn start(directory: PathBuf) -> Arc<FakeDaemon> {
        let socket = std::env::temp_dir().join(format!("ocpty-fake-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&socket).expect("bind fake daemon socket");
        let fake = Arc::new(FakeDaemon {
            directory,
            socket,
            state: Mutex::new(State {
                token: "tok".into(),
                protocol: crate::frame::PROTOCOL_VERSION,
                instance: "inst-1".into(),
                next_id: 1,
                handoff_ttl: 60_000,
                ticket: "tick".into(),
                ..Default::default()
            }),
            auth_failures: AtomicUsize::new(0),
            owner_close: Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        fake.write_registration();
        let weak = Arc::downgrade(&fake);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let Some(fake) = weak.upgrade() else { return };
                if fake.closed.load(Ordering::SeqCst) {
                    return;
                }
                tokio::spawn(async move { fake.serve(stream).await });
            }
        });
        fake
    }

    /// Stops serving: the socket goes away and the owner connection closes (the registration stays).
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket);
        self.owner_close.notify_waiters();
    }

    /// Rewrites `service.json` from the current state.
    pub fn write_registration(&self) {
        std::fs::create_dir_all(&self.directory).unwrap();
        let state = self.state.lock();
        let registration = json!({
            "instance_id": state.instance, "pid": PID, "protocol": state.protocol,
            "socket": self.socket.to_string_lossy(), "token": state.token, "features": [],
        });
        std::fs::write(self.directory.join("service.json"), registration.to_string()).unwrap();
    }

    /// A handoff for this fake, as a previous server would have passed it.
    pub fn handoff(&self) -> crate::Handoff {
        crate::Handoff {
            directory: self.directory.to_string_lossy().into_owned(),
            instance_id: self.state.lock().instance.clone(),
            ticket: "ticket".into(),
            expires_at: crate::daemon::now_ms() + 60_000,
        }
    }

    /// Adds a terminal.
    pub fn add_terminal(&self, group: &str) -> u64 {
        let mut state = self.state.lock();
        let id = state.next_id;
        state.next_id += 1;
        state.processes.push(Process { info: terminal(id, group) });
        id
    }

    pub fn requests(&self) -> Vec<Value> {
        self.state.lock().requests.clone()
    }

    /// The requests of one op.
    pub fn ops(&self, op: &str) -> Vec<Value> {
        self.state.lock().requests.iter().filter(|r| r["op"] == op).cloned().collect()
    }

    /// The signals sent to a process.
    pub fn signals(&self, id: u64) -> Vec<String> {
        self.ops("signal")
            .into_iter()
            .filter(|r| r["id"] == id)
            .filter_map(|r| r["signal"].as_str().map(str::to_string))
            .collect()
    }

    pub fn process(&self, id: u64) -> Option<Value> {
        self.state.lock().processes.iter().find(|p| p.id() == id).map(|p| p.info.clone())
    }

    pub fn waiters(&self, id: u64) -> usize {
        self.state.lock().waiters.get(&id).map_or(0, |w| w.iter().filter(|s| !s.is_closed()).count())
    }

    /// Ends a running process.
    pub fn exit(&self, id: u64, exit_code: Option<u64>, signal: Option<&str>) {
        let mut state = self.state.lock();
        let Some(found) = state.processes.iter_mut().find(|p| p.id() == id) else { return };
        if !found.running() {
            return;
        }
        let mut lifecycle = json!({"status": "exited", "exit_code": exit_code});
        if let Some(signal) = signal {
            lifecycle["signal"] = json!(signal);
        }
        found.info["lifecycle"] = lifecycle;
        found.info["exited_at"] = json!(now_ms());
        let reply = exited(&found.info);
        for waiter in state.waiters.remove(&id).unwrap_or_default() {
            let _ = waiter.send(Some(reply.clone()));
        }
    }

    /// Appends to the output file and counts it, as the daemon's reader does; after [`Self::fail`], the file stops
    /// growing.
    pub fn output(&self, id: u64, text: &str) {
        let mut state = self.state.lock();
        let Some(found) = state.processes.iter_mut().find(|p| p.id() == id) else { return };
        if found.info.get("pipe").is_none() {
            return;
        }
        if found.info["pipe"].get("output_error").is_none_or(Value::is_null) {
            use std::io::Write;
            let path = found.info["pipe"]["output_path"].as_str().unwrap_or_default().to_string();
            let mut file = std::fs::OpenOptions::new().append(true).create(true).open(path).unwrap();
            file.write_all(text.as_bytes()).unwrap();
            let lines = found.info["pipe"]["output_lines"].as_u64().unwrap_or(0) + text.matches('\n').count() as u64;
            found.info["pipe"]["output_lines"] = json!(lines);
        }
        let tail = found.info["output_tail"].as_u64().unwrap_or(0) + text.len() as u64;
        found.info["output_tail"] = json!(tail);
    }

    /// The output file stops: later output is counted but not written.
    pub fn fail(&self, id: u64, message: &str) {
        let mut state = self.state.lock();
        if let Some(found) = state.processes.iter_mut().find(|p| p.id() == id)
            && found.info.get("pipe").is_some()
        {
            found.info["pipe"]["output_error"] = json!(message);
        }
    }

    /// The daemon going away under a waiting server: the wait connections close without a reply.
    pub fn drop_waiters(&self, id: u64) {
        let waiters = self.state.lock().waiters.remove(&id).unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(None);
        }
    }

    async fn serve(self: Arc<Self>, mut stream: UnixStream) {
        let mut owner = false;
        loop {
            let frame = {
                let mut reader = FrameReader::new(&mut stream);
                let read = if owner {
                    tokio::select! {
                        _ = self.owner_close.notified() => return,
                        frame = reader.next() => frame,
                    }
                } else {
                    reader.next().await
                };
                match read {
                    Ok(Some(frame)) => frame,
                    _ => return,
                }
            };
            let Ok(envelope) = serde_json::from_slice::<Value>(&frame) else { return };
            let request = envelope["request"].clone();
            let op = request["op"].as_str().unwrap_or_default().to_string();
            if op != "ping" && !owner && self.auth_failures.load(Ordering::SeqCst) > 0 {
                self.auth_failures.fetch_sub(1, Ordering::SeqCst);
                self.state.lock().token = "rotated".into();
                self.write_registration();
                let _ = send(&mut stream, json!({"type": "error", "message": "authentication failed"})).await;
                return;
            }
            if envelope["token"] != Value::String(self.state.lock().token.clone()) {
                let _ = send(&mut stream, json!({"type": "error", "message": "authentication failed"})).await;
                return;
            }
            self.state.lock().requests.push(request.clone());
            if self.state.lock().hang_up.contains(&op) {
                return;
            }
            if op == "own" {
                owner = true;
                if send(&mut stream, json!({"type": "owned"})).await.is_err() {
                    return;
                }
                continue;
            }
            if op == "subscribe" {
                self.subscribe(&mut stream, &request).await;
                return;
            }
            if op == "wait" {
                let receiver = {
                    let mut state = self.state.lock();
                    let found = state.processes.iter().find(|p| p.id() == request["id"].as_u64().unwrap_or(0)).cloned();
                    match found {
                        Some(found) if found.info.get("pipe").is_some() => {
                            if found.running() {
                                let (tx, rx) = oneshot::channel();
                                state.waiters.entry(found.id()).or_default().push(tx);
                                Err(rx)
                            } else {
                                Ok(exited(&found.info))
                            }
                        }
                        _ => Ok(json!({"type": "error", "message": format!("terminal {} not found", request["id"])})),
                    }
                };
                match receiver {
                    Ok(reply) => {
                        let _ = send(&mut stream, reply).await;
                    }
                    Err(rx) => {
                        // A closed client end cancels the wait.
                        let mut probe = [0u8; 1];
                        tokio::select! {
                            reply = rx => {
                                if let Ok(Some(reply)) = reply {
                                    let _ = send(&mut stream, reply).await;
                                }
                            }
                            _ = tokio::io::AsyncReadExt::read(&mut stream, &mut probe) => {}
                        }
                    }
                }
                return;
            }
            let reply = self.handle(&op, &request);
            if send(&mut stream, reply).await.is_err() || !owner {
                return;
            }
        }
    }

    fn handle(&self, op: &str, request: &Value) -> Value {
        let mut state = self.state.lock();
        let id = request["id"].as_u64().unwrap_or(0);
        let not_found = || json!({"type": "error", "message": format!("terminal {} not found", request["id"])});
        let index = state.processes.iter().position(|p| p.id() == id);
        match op {
            "ping" => {
                json!({"type": "pong", "instance_id": state.instance, "pid": state.pong_pid.unwrap_or(PID), "protocol": state.protocol, "features": []})
            }
            "prepare_handoff" => json!({"type": "handoff", "ticket": state.ticket, "expires_at": now_ms() + state.handoff_ttl}),
            "list" => {
                let processes: Vec<Value> =
                    state.processes.iter().filter(|p| p.info["mode"] == request["mode"]).map(|p| p.info.clone()).collect();
                json!({"type": "processes", "processes": processes})
            }
            "get" => match index {
                Some(i) => json!({"type": "process", "process": state.processes[i].info}),
                None => not_found(),
            },
            "create_pty" => {
                let id = state.next_id;
                state.next_id += 1;
                let created = terminal(id, request["group_id"].as_str().unwrap_or_default());
                state.processes.push(Process { info: created.clone() });
                json!({"type": "created", "process": created})
            }
            "create_pipe" => {
                if !request["cwd"].as_str().is_some_and(|c| Path::new(c).is_absolute()) {
                    return json!({"type": "error", "message": "cwd must be absolute"});
                }
                if let Some(message) = &state.fail_create {
                    return json!({"type": "error", "message": message});
                }
                let id = state.next_id;
                state.next_id += 1;
                let created = pipe(id, request);
                let _ = std::fs::write(request["output_path"].as_str().unwrap_or_default(), "");
                state.processes.push(Process { info: created.clone() });
                json!({"type": "created", "process": created})
            }
            "shutdown" => {
                // Unregisters, so the server sees it stopped; it keeps serving connections already open.
                let _ = std::fs::remove_file(self.directory.join("service.json"));
                json!({"type": "ok"})
            }
            "rename" => match index {
                Some(i) if state.processes[i].info.get("pty").is_some() => {
                    let title = request["title"].as_str().filter(|t| !t.is_empty());
                    let pty = &mut state.processes[i].info["pty"];
                    pty["user_title"] = json!(title);
                    pty["title"] = json!(title.unwrap_or("Terminal"));
                    json!({"type": "ok"})
                }
                _ => json!({"type": "error", "message": "terminal not found"}),
            },
            "signal" => match index {
                Some(i) if state.processes[i].info.get("pipe").is_some() => {
                    let signal = request["signal"].as_str().unwrap_or_default().to_string();
                    if signal != "SIGTERM" || !state.trap.contains(&id) {
                        drop(state);
                        self.exit(id, None, Some(&signal));
                    }
                    json!({"type": "ok"})
                }
                _ => not_found(),
            },
            "set_metadata" => match index {
                Some(i) if state.processes[i].info.get("pipe").is_some() && request["metadata"].is_object() => {
                    state.processes[i].info["pipe"]["metadata"] = request["metadata"].clone();
                    json!({"type": "ok"})
                }
                _ => not_found(),
            },
            "terminate" => match index {
                Some(i) => {
                    state.processes.remove(i);
                    for waiter in state.waiters.remove(&id).unwrap_or_default() {
                        let _ = waiter.send(Some(json!({"type": "error", "message": "terminated"})));
                    }
                    json!({"type": "ok"})
                }
                None => not_found(),
            },
            "write" | "resize" | "control" | "input" => json!({"type": "ok"}),
            _ => json!({"type": "error", "message": format!("unsupported request: {op}")}),
        }
    }

    /// A scripted subscription: the replay `hello`, output `abc`, a title change, a stray `ok` and the exit.
    async fn subscribe(&self, stream: &mut UnixStream, request: &Value) {
        let process = self.process(request["id"].as_u64().unwrap_or(0)).unwrap_or_else(|| terminal(1, "ses_a"));
        let attached = json!({
            "type": "attached", "process": process, "role": request["role"], "generation": 1,
            "requested_offset": request["offset"], "available_offset": 0, "end_offset": 5, "truncated": false,
            "replay_base64": "aGVsbG8="
        });
        let _ = send(stream, attached).await;
        let _ = stream.write_all(&encode_payload(&output_frame_payload(5, 8, b"abc")).unwrap()).await;
        let _ = send(stream, json!({"type": "title_changed", "title": "vim"})).await;
        let _ = send(stream, json!({"type": "ok"})).await; // ignored by the pump
        let _ = send(stream, json!({"type": "exited", "exit_code": 0, "final_offset": 8, "exited_at": now_ms()})).await;
    }
}

async fn send(stream: &mut UnixStream, value: Value) -> std::io::Result<()> {
    stream.write_all(&encode(&value).unwrap()).await
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}
