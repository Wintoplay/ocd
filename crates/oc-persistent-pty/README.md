# oc-persistent-pty

Client side of opencode's process daemon, a port of `packages/core/src/persistent-pty/**` of 2.3.1
(plus the handoff sidecar from `packages/client/src/pty-handoff.ts`). Persistent terminals and pipe
processes (the shell service's commands, see `oc-tools`) live in the external MIT Rust daemon
`opencode-pty` (npm `@opencode/pty@0.6.0`, protocol 9; upstream vendors its sources as
`packages/process-daemon`), so they survive server restarts. Spec: `docs/spec/08-mcp-pty-plugins-misc.md`
§2.5–§2.9.

## Modules

| module | upstream | contents |
|---|---|---|
| `binary` | `binary.bun.ts`, `cli/script/opencode-pty.ts` | `AVAILABLE`/`available()`, `BinaryResolver`, the pinned npm packages, install/validate, tar/gzip extraction |
| `daemon` | `daemon.ts` | `DaemonTransport`: spawn, discover, owner connection, one-shot requests, retry policy, subscribe, shutdown, handoff, adoption |
| `frame` | `daemon.ts:713-745` | length-prefixed framing, `FrameDecoder`, `FrameReader`, output frames |
| `wire` | `daemon.ts:11-139` | `ProcessInfo` (`mode`, `pty`/`pipe` parts, `started_at`/`exited_at`), `Lifecycle` (with `signal`), `Registration`, `WireResponse`, `Exited`, `StreamEvent`, `Role` |
| `types` | `@opencode/schema/persistent-pty` | `Info`, `Snapshot`, `ReadResult`, `CreateInput`, `UpdateInput`, `BusEvent`, `to_id`/`from_id` |
| `handoff` | `schema Handoff`, `cli/src/server-process.ts`, `client/src/pty-handoff.ts` | `Handoff`, `OPENCODE_PTY_HANDOFF`, the `.pty-handoff` sidecar |
| `ws` | `server/src/handlers/persistent-pty.ts` | WebSocket message builders and the framed-input decoder |
| `testing` (feature `testing`, Unix) | `test/fixture/persistent-pty-daemon.ts` | `FakeDaemon`: an in-process protocol-9 daemon with terminals and pipe processes (exit, output, write errors, dropped waits, signals, traps) for this crate's and `oc-tools`' tests |

## Service API (`PersistentPty`, cheap to `Clone`)

```rust
let pty = PersistentPty::new(&global, Options { handoff: handoff::take_from_env(), ..Default::default() }).await;
```

`Options { handoff, root, binary, shell, http, id }` all default sensibly: `root` = `<global.state>/pty`,
`binary` = memoized `BinaryResolver` over `global.bin`, `shell` = `shell_environment(global.bin)`,
`id` = random UUID. `new` never fails. If the inherited handoff can't be adopted, the crate logs a warning
and starts fresh. The daemon is spawned lazily on the first `create`.

All methods are `async` and return `Result<_, Error>`. `Error::Unavailable(message)` maps to HTTP
`ServiceUnavailableError {message, service: "opencode-pty"}`. `Error::NotFound(ptyID)` maps to
`PtyNotFoundError` (`Display` = `PTY session not found: <id>`). Every terminal method looks the terminal up
with `{op:"get", id}` first; an ID that is not `pty_persistent_<n>`, or a pipe process, is `NotFound`.

| method | endpoint (`{data: …}` wrapping is the server's job) |
|---|---|
| `list(Some(session_id))` → `Vec<Info>` (`[]` if no daemon) | `GET /api/experimental/session/:sessionID/terminal` |
| `create(session_id, CreateInput)` → `Info` (publishes `persistent-pty.added`); `cwd` (default `/`) is resolved like `path.resolve` and must be a directory, else `Error::InvalidCwd` ("cwd is not a directory: <cwd>", HTTP 400 `kind: "invalid_cwd"`) before the daemon is contacted | `POST /api/experimental/session/:sessionID/terminal` |
| `read(session_id, lines: Option<i64>)` → `Option<ReadResult>` | `GET …/session/:sessionID/terminal/read?lines=` |
| `shutdown()` | `POST /api/experimental/persistent-pty/shutdown` (204) |
| `handoff()` → `Option<Handoff>` | `POST /api/experimental/persistent-pty/handoff` → `{handoff}` |
| `get(id)` → `Info` | `GET /api/experimental/persistent-pty/:ptyID` |
| `resize(id, cols, rows, attachment_id)` then `get(id)` | `PUT /api/experimental/persistent-pty/:ptyID` (`UpdateInput`) |
| `snapshot(id)` → `Snapshot` (serializes `checkpoint` as base64) | `GET …/:ptyID/snapshot` |
| `rename(id, title)` (`None` clears; publishes `persistent-pty.updated`) | `PATCH …/:ptyID` with `title` |
| `remove(id)` (publishes `persistent-pty.removed`) | `DELETE …/:ptyID` (204) |
| `remove_session(sessionID)` (every `session.deleted`; NotFound skipped, other failures logged) | — |
| `get(id)` before issuing a ticket | `POST …/:ptyID/connect-token` |
| `attach(id, AttachInput)` + `control` / `input` | `GET …/:ptyID/connect` (WebSocket) |
| `write(id, bytes, attachment_id)` | (plugin/internal) |

Pipe processes (`createPipe` … `listPipes`), with `PipeError::{Unavailable, Request, NotFound}`:
`Unavailable` means the daemon could not be reached or started and the request never ran (spawning the command
elsewhere is safe); `Request` means the daemon rejected it or its reply was lost after sending (it may have run).

| method | daemon request |
|---|---|
| `create_pipe(CreatePipeInput {program, args, cwd, env, group_id, output_path, metadata})` → `PipeInfo` (starts the daemon) | `create_pipe` |
| `wait_pipe(id)` → `PipeExit {exit_code, signal, final_offset, output_lines, output_error, exited_at}`: its own connection, no timeout, never starts the daemon; dropping the future closes it; a daemon that goes away is `Unavailable` | `wait` |
| `get_pipe(id)` (`NotFound` for an unknown id, a terminal, or no daemon) | `get` |
| `signal_pipe(id, Signal::{Term, Int, Kill})`, `set_pipe_metadata(id, metadata)`, `terminate_pipe(id)` | `signal`, `set_metadata`, `terminate` |
| `list_pipes()` (never starts the daemon; empty without one) | `list {mode:"pipe"}` |

Other methods:

- `handoff_prepared()` is `handoffPrepared()`: whether a prepared handoff's ticket is still valid (this server is
  being replaced, so the daemon's processes are left to the next one). `shutdown()` clears it.
- `pipes_available()`: whether pipe processes can run without downloading the binary first (a daemon is owned,
  the binary was injected, or `BinaryResolver::installed()` finds it). `prefetch()` installs the pinned binary in
  the background once per process (the managed service calls it at boot).

- `subscribe_events()` returns a `broadcast::Receiver<BusEvent>`. Use `BusEvent::event_type()`
  (`persistent-pty.added`, `persistent-pty.removed` or `persistent-pty.updated`) and `properties()` (`{sessionID, terminal}` or
  `{sessionID, ptyID}`) to forward to the bus or SSE.
- `available()` returns `true` on every platform (since 2.1; `false` on Windows before), which is the value of
  `GET /api/info` → `capabilities.persistentPty`.
- `close()` drops the owner connection (the scope finalizer). The daemon then stops unless a
  handoff was prepared. Dropping the last clone has the same effect.
- `daemon()` gives access to the `DaemonTransport` (`id()`, `directory()`, `is_owner()`, `discover()`).

### WebSocket (`persistentPty.connect`)

```rust
let mut a = pty.attach(&id, AttachInput { cursor, attachment_id, role, takeover }).await?; // Err → close 4404 "terminal unavailable"
send_text(ws::attached_message(&attachment_id, framed, &a));
if !a.replay.data.is_empty() { send_binary(a.replay.data.clone()) }
send_text(ws::replay_complete_message(a.replay.end_offset));
let mut events = a.activate().unwrap();           // EventStream: futures::Stream<Item = StreamEvent>
// each event → ws::outbound(event): Output → binary raw bytes; others → JSON text
//   (StreamEvent::to_ws_json(): exited/controller_changed/title_changed/foreground_process_changed/resized+checkpoint)
// stream end → close 1000
// inbound: legacy → pty.input(id, att, info.size.cols, info.size.rows, bytes)
//          framed → ws::FramedInput::decode(bytes): Control → pty.control(..), Input → pty.input(..), None → ignore
a.detach(); // on socket close
```

An `exited` event seen by any attachment removes the terminal in the background
(`removeVisibleExit`). A controller attach, or any `resize`, `control` or `input`, selects the session's
"current" terminal for `read`. The selection is in memory only.

### Mapping to `oc-server`'s `PersistentPtyEvent`

- `Output { data, .. }` → `Output(data.into())`
- `Resized { cols, rows, generation, checkpoint }` → `Resized { … }`
- every other event → `Json(event.to_ws_json().unwrap())`

`PersistentPtyControl::input/control` call `pty.input/control(id, attachment_id, …)`.
`detach` calls `Attachment::detach` (or a cloned `DetachHandle`). Convert `Info` to the generated
`PersistentPtyInfo` with a `serde_json` round trip, because the JSON is identical.

## Binary resolution

`BinaryResolver::resolve()`:

1. A non-empty `OPENCODE_PTY_BIN` wins.
2. Unsupported targets fall back to `opencode-pty` on `PATH`.
3. Otherwise the crate installs into `<global.bin>/opencode-pty/0.6.0-<sha256[0..16]>/opencode-pty` (`opencode-pty.exe`
   on Windows). This is
   the same layout and validation as upstream's embedded asset: 0700 private dirs, no symlinks, owned by
   the current uid, sha256-verified, chmod 755, atomic `opencode-pty.<pid>.<uuid>.tmp` + rename, and the
   same error strings.

When the binary isn't cached, the crate downloads
`<registry>/@opencode/pty-<os>-<arch>[-gnu|-musl]/-/pty-…-0.6.0.tgz` (`<os>` is `darwin`, `linux` or `win32`). The
registry is `$npm_config_registry` or the public registry. The download is checked against the pinned npm `integrity`
(sha512), `package/bin/opencode-pty` (`opencode-pty.exe` on Windows) is extracted in memory, and its pinned sha256 is checked before install. The platform is chosen from the
build target, the same as upstream (musl targets use `-musl`).

## Restart handoff

- Old server: `pty.handoff()` sends `prepare_handoff` on the owner connection and returns
  `{directory, instanceID, ticket, expiresAt}`.
- CLI: `handoff::prepare(file, &source, request_handoff, shutdown)` writes `<service-file>.pty-handoff`.
  `handoff::environment(file)` then gives the `OPENCODE_PTY_HANDOFF` value for the replacement. `complete` and
  `clear` are also provided.
- New server: `handoff::take_from_env()` reads and deletes the variable and warns on invalid JSON. Pass the
  result as `Options::handoff`. On construction the transport checks the expiry and the instance ID, takes
  the daemon's directory name as its ID (the daemon keeps its directory: 2.3.1 no longer moves older
  releases' directories), then sends `own` with the ticket. Any failure logs `Discarding PTY restart
  handoff; persistent terminals will start fresh`, sends `shutdown` to that daemon and starts in a new UUID
  directory.
- Coexistence: each daemon owns `<root>/<uuid>`. A daemon of another protocol (an older ocd's 7, upstream
  2.3.0's 8) is never used, replaced or killed: its registration fails `protocol`, and a server starts its own
  daemon in a fresh directory.

## Daemon protocol (as implemented)

- Spawn: `opencode-pty daemon --runtime-dir <root> --name <id>`, detached (`setsid`) with null stdio. The
  transport polls `discover` every 50 ms for up to 5 s, then claims ownership. On failure it sends SIGTERM and
  retries once in a fresh directory, except when the binary could not be spawned or registered with another
  protocol under the child's own pid: that binary is never started again by this transport.
- Frames: `u32` BE length + payload ≤ 8 MiB. Requests are `{"token","request"}`. Output frames are
  `0x00 | start u64 | end u64 | data`.
- One-shot requests use a new socket each and have a 5 s timeout. An `authentication failed` response retries
  once after rediscovery. A connect error drops the registration and, for start-required requests
  (`create_pty`, `create_pipe`), respawns; a request whose reply was lost after sending is never replayed
  (`Response`). Any other `error` reply is a `Request` error. `requestIfRunning` maps connect errors to "no daemon".
- The owner connection is one long-lived socket with a 5 s claim deadline. When it closes, the owner and the
  registration are cleared, and the next request rediscovers and reclaims.
- `shutdown` sends `shutdown`, then polls until `discover` fails (5 s, `opencode-pty did not stop`).

## Not implemented / divergences

- Upstream embeds the binary in its executable. This port downloads it once instead.
- `write` takes bytes, not a JS string. The HTTP handlers only use `input` and `control`.
- I/O error texts come from Rust's `std::io::Error`, not Node's `ENOENT: …` messages.
- Windows: the daemon listens on a named pipe (`\\.\pipe\opencode-pty-…`), which the client opens with tokio's
  named-pipe client (a busy pipe is retried for up to 30 s, like libuv), and the daemon is spawned detached
  (`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, no window), like Node's `detached: true, windowsHide: true`.
  This compiles for `x86_64-pc-windows-gnu` but has not been run on Windows.

## Tests

- Unit tests: framing, protocol-9 wire decoding (processes, `exited` with `signal`/`output_lines`, required
  `features`), event JSON, IDs, `Info`/`Snapshot`/`ReadResult` JSON, tar/gzip, install validation, handoff/sidecar.
- `tests/fake_daemon.rs`: 17 tests against the `testing` fake, ported from upstream's `persistent-pty-daemon.test.ts`
  and `persistent-pty-session.test.ts`: ownership, list/get, subscribe and the pump, auto-remove on exit, handoff and
  `handoffPrepared` (and its expiry), owner-loss recovery, auth retry and rotated registrations (also for
  subscriptions), protocol and registration mismatches, incompatible daemons left alone and an incompatible binary
  never started twice, no replay of a dispatched request, `wait` on its own connection (interrupted, daemon gone),
  the pipe methods, adoption and discarded handoffs, rename, session removal.
- `tests/real_daemon.rs`: 4 ignored tests against the real daemon. The binary comes from `OPENCODE_PTY_BIN` or is
  downloaded through `BinaryResolver`. They cover create, attach, input, read, snapshot, resize, write, handoff
  adoption across two service instances, remove, auto-remove on exit, shutdown, crash recovery, the daemon stopping
  with its owner, rename, and pipe processes (output file and line count, exit code, SIGTERM, metadata, list/get/
  terminate, and the daemon's SIGTERM to the process group of a command that exits non-zero).

  Run them with `cargo test -p oc-persistent-pty --test real_daemon -- --ignored`.
