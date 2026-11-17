# oc-codemode

Rust port of opencode's `@opencode/codemode` (upstream `packages/codemode`, plus the model-facing glue from
`packages/core/src/codemode`): a confined, tree-walking interpreter for a JavaScript subset that lets a model script
host tools from a single `execute` tool call.

Programs can only use the supported JavaScript built-ins, the `tools` tree, the `search(...)` discovery function
(synchronous, and its result can also be awaited or chained like a Promise), and globals the host adds through extensions (core adds only `fetch`). `import`, `import()` and
`require` load those globals by Node module name (see Modules). Classes, getters/setters, BigInt, arbitrary symbols,
timers, exports, `eval`/`Function`, and TypeScript syntax are rejected with the same diagnostics as upstream. The exact support matrix is upstream's `interpreter-support.md`; behaviour, diagnostics, and model-visible
strings match upstream byte for byte (verified by replaying upstream's recorded test executions, see Tests).

## Architecture

| Module | Role (upstream source) |
| --- | --- |
| `parser` | Port of acorn 8.15 (`ecmaVersion: "latest"`, script, top-level `return`/`await`, `allowImportExportEverywhere`), including its regex validator; exact acorn error messages and UTF-16 positions |
| `ast` | ESTree-shaped AST |
| `value`, `object`, `number` | Values (UTF-16 strings, f64, two confined symbols), objects with prototype chains and JS property order, ECMAScript number formatting/parsing (`objects.ts`) |
| `interp::eval` | Evaluator (`interpreter.ts`): statements, expressions, closures, destructuring, generators, async functions |
| `interp::runner` | The job queue (`runner.ts`): promise jobs in V8 order, promise states and reactions, `await`, host work settling through an inbox, blocking host calls, stall detection, unhandled-rejection warnings |
| `interp::promises`, `generators`, `iteration`, `coerce`, `callback`, `errors`, `globals`, `extensions`, `modules` | Promise built-ins and combinators, generator state machines, iterator cursors and CreateAsyncFromSyncIterator, ToPrimitive, callback admission, diagnostics, global table, host-value boundary, `import`/`require` module resolution (`modules.ts`) |
| `stdlib::*` | Standard library (`stdlib/*.ts`): Object, Array, String, RegExp (`regress`), Number, Math, Date, JSON, Map/Set/Weak*, Iterator, URL/URLSearchParams, Headers, Uint8Array, TextEncoder/Decoder (`decode(bytes, { stream: true })` keeps a split character for the next chunk and strips only a stream's first BOM, 2.3.0), atob/btoa, crypto, structuredClone, console |
| `data` | JSON boundary (`data.ts`): `toJson`/`toBoundary`/`fromJson`, JS-compatible JSON text |
| `tool_runtime`, `tool_schema` | Tool trie, `search`, admission/limits/hooks (`tool-runtime.ts`); JSON Schema → TypeScript signatures (`tool-schema.ts`) |
| `codemode` | Public runtime API, limits, output bounding (`codemode.ts`, `interpreter/execute.ts`) |
| `catalog`, `execute_tool`, `web` | Core glue: catalog summarization and instruction texts (`core/src/codemode/{catalog,instructions}.ts`), the `execute` tool's description/schema/result formatting/progress rows (`codemode/tool.ts`), and the `fetch` extension (`codemode/fetch.ts`) |
| `openapi` | OpenAPI 3.x → tools adapter (`openapi/*.ts`; unused by core upstream) |

Each execution runs on a dedicated thread (1 GiB stack, so deep recursion up to the 10 000-call limit is safe) with
its own job queue. Program code runs only inside a job, each to completion: the program body is the first job,
promise reactions and resumed awaits are queued in V8's order, and an async body (async function, async generator,
the program) is a Rust future that parks at an `await` and is polled again by the job that resumes it. Host work
(tool calls, async extension globals such as `fetch`) runs as host futures polled on that thread inside the caller's
tokio runtime context; they wake the interpreter through a thread-safe queue and settle their promise in a later
turn, after the jobs already queued. `search(...)` and synchronous extension globals hold the current job. A program
stuck at an `await` with nothing queued and no host work running fails at once, pointing at that `await`. When the
program returns, host work still running is dropped (tool `after` hooks see `Interrupted`), and all objects of the
run are torn down to release reference cycles.

## Public API

### Runtime

```rust
use std::sync::Arc;
use oc_codemode::codemode::{CodeMode, ExecutionLimits, ExecResult};
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};

let runtime = CodeMode::new(
    vec![
        ToolEntry::Namespace { path: "orders".into(), description: Some("Purchases and shipments".into()) },
        ToolEntry::Tool(
            ToolDef::new("orders.lookup", "Look up an order by ID", serde_json::json!({
                "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"]
            }))
            .with_output(serde_json::json!({ "type": "object", "properties": { "status": { "type": "string" } } })),
        ),
    ],
    Arc::new(my_host),                          // impl oc_codemode::host::Host
    vec![oc_codemode::web::extension(Arc::new(oc_codemode::web::ReqwestClient::new()))], // `fetch`
    ExecutionLimits::default(),                 // timeout_ms / max_tool_calls / max_output_bytes: None = unlimited
)?;

let catalog = runtime.catalog();                // Vec<ToolDescription { path, description, signature }>, sorted by path
let result: ExecResult = runtime.execute("return await tools.orders.lookup({ id: 'o1' })").await;
let json = result.to_json();                    // upstream CodeMode.Result shape and key order
```

- `CodeMode::new` validates limits (`timeoutMs must be a safe integer greater than or equal to 1.` …), rejects empty
  tool-name segments, and rejects extension globals that shadow built-ins (`Extension "x" global "y" is already
  defined.`) — returned as `CodeModeError::{Range, Type}`.
- `CodeMode::execute(&self, code) -> ExecResult` is a `Send` future; dropping it interrupts the program.
  `execute_blocking` runs it on a private current-thread runtime.
- `ExecResult::{Success { value, warnings, logs, truncated, tool_calls }, Failure { error, logs, truncated,
  tool_calls }}`; `Diagnostic { kind: DiagnosticKind, message, location: Option<Location { line, column }>,
  suggestions }` with the eleven upstream kinds (`ParseError`, `UnsupportedSyntax`, `UnknownTool`,
  `InvalidToolInput`, `InvalidToolOutput`, `InvalidDataValue`, `ToolCallLimitExceeded`, `TimeoutExceeded`,
  `ToolFailure`, `ExecutionFailure`, `Truncated`).
- Tool paths: dots create namespaces; the last tool for a path wins; a path may be both a tool and a namespace.
  `ToolDef.input`/`output` are `tool_schema::ToolSchema::Json` (render-only JSON Schema) or `ToolSchema::Document`
  (a pre-built schema document with top-level definitions). `output: None` renders `Promise<void>` and the program
  receives `undefined`.
- `search({ query?, namespace?, limit?, offset? })` ranks tools as 2.3.0 does (#62984): field-weighted BM25 (k1 1.2,
  b 0.75; the tool's name 3, its namespace 2, description 1, namespace descriptions 0.4, parameter names 0.3) over
  whole words (split at case changes and non-alphanumerics, camelCase words also whole, plural endings stemmed), a
  4+-letter query word also matching longer words it starts with at 0.3, a boost for short names, ties by path. The
  same ranking picks an unknown tool's "Did you mean …?", and calling a namespace names up to three of its tools
  ranked by the namespace's last segment (`Tool 'x' is not callable, it is a namespace. Did you mean tools.x.a,
  tools.x.b, or tools.x.c?`, suggestion `N more under 'x'. Use search({ namespace: "x" }) to list them.`, #63697).
- Helpers: `tool_runtime::tool_expression(path)` (`tools.a["b-c"]`), `tool_runtime::SEARCH_SIGNATURE`,
  `tool_runtime::signature_of(&ToolDef)`, `tool_schema::{json_schema_to_typescript, schema_to_typescript,
  input_properties, is_empty_input}`.

### Host interface (`host`)

```rust
pub trait Host: Send + Sync + 'static {
    /// One admitted tool call; `path` is the canonical dotted path ("ns.tool"), `input` the decoded input
    /// (None = `undefined`). Ok(None) = `undefined`.
    fn call(&self, path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>>;
    /// Validation before admission; Err(msg) → InvalidToolInput "Invalid input for tool '<path>': <msg>".
    fn decode_input(&self, path: &str, input: Option<Json>) -> Result<Option<Json>, String> { Ok(input) }
    fn tool_before(&self, call: &ToolInvocation) -> BoxFuture<'static, Result<(), ToolCallError>>;   // deny = Err
    fn tool_after(&self, call: &ToolInvocation, result: &CallResult) {}
    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>>;
    fn extension_after(&self, call: &ExtensionInvocation, result: &CallResult) {}
}
```

`ToolCallError::Failure(msg)` → `ToolFailure`; `ToolCallError::InvalidOutput(msg)` → `InvalidToolOutput: Invalid
output from tool '<name>': <msg>`. Hooks also run around `search` (as tool `search`), which counts toward
`max_tool_calls`. `ToolInvocation { id, name, input }`, `CallResult::{Success, Failure, Interrupted}`.
`NoTools` is a host without tools.

Extensions (`host::Extension`, `SimpleExtension`) expose `HostFunction`s as globals. The `extension_before` hook
runs before the host function, so a denied call never reaches the host: a host function marked asynchronous
(`HostFunction::asynchronous`, `host::host_fn_async`; upstream's `async` functions and `Extension.async`, e.g. `fetch`)
always returns a promise, which a denial rejects, while a synchronous one throws. Values cross as `HostValue`
copies (plain data, Date, RegExp, URL, URLSearchParams, Headers, Map, Set, bytes, errors with name/message/cause/own
fields, and nested host functions that become callable program functions). `HostValue::Live` is a host object with
accessors read from the host on every access (non-enumerable) and `Symbol.asyncIterator` methods (`HostKey`,
`HostProperty`); `fetch` builds its streaming responses from it. A host function returns
`HostCall::Ready` (synchronous) or `HostCall::Pending` (the program sees a promise).

HTTP: `host::{HttpClient, HttpRequest, HttpResponse}`; `HttpClient::send_streaming` returns an
`HttpStreamingResponse` once the head arrives, its body a `BodyStream` of chunks (the default buffers `send`);
`web::ReqwestClient` is the default client. `web::extension(client)` is the `fetch` extension exposing `fetch(url, {
method?, headers?, body? })` with a 30 s timeout (body included) and, since 2.3.0 (#63436), a streaming response like
Bun's: `{ url, status, statusText, ok, headers, redirected, body, bodyUsed, text(), json(), bytes(), arrayBuffer() }`.
`headers` is a real `Headers`; `body` is `null` for HEAD, 204, 205 and 304, else a stream with `locked`, `cancel()`,
`getReader()` (`{ read, cancel, releaseLock }`; `getReader({ mode })` is `TypeError: Only the default stream reader is
supported.`) and `for await` iteration; the whole-body readers consume it once (`TypeError: Body already used`, as
are a locked or read stream's), `arrayBuffer()` gives a `Uint8Array`, and `bodyUsed` follows Bun (set by a reader too).
Bun's lock and detached-reader errors and its JSON errors for an empty body are reproduced; a timeout is
`Error: The operation timed out.` (Bun's `TimeoutError` crossing). A body still open when the program ends is
cancelled with the run. `web::extension_with(client, Some(authorize))` checks each call after the `file:` refusal
and before anything is sent (core asserts the `webfetch` permission, `codemode/fetch.ts`); an `Err` rejects the
program's promise. Since 2.2.7 the URL loses its `#fragment` before any check, a URL with credentials is a
`TypeError` (`fetch cannot be used with credentials in the URL.`), and `web::extension_checked(client, authorize,
check_redirect)` follows redirects itself (give it `ReqwestClient::manual()`): up to 20 hops under one 30 s deadline,
refusing non-http(s) targets and targets with credentials, discarding each redirect's body, checking each target
(without its fragment) with `check_redirect(next, args, url)`, and rewriting the request as a browser does (303, or 301/302 after a POST, become a
GET without body or content headers; another origin loses `authorization`, `cookie` and `proxy-authorization`). The
response's `url` is the last target as written and `redirected` whether any hop was followed. `ProgressRows::interrupt_running` settles the rows of calls still running as errors when an
execution is interrupted.

### The `execute` tool and catalog instructions

- `execute_tool::{NAME, DESCRIPTION, input_schema()}` — the tool definition (verbatim upstream description).
- `execute_tool::format_result(&ExecResult)` — the text output (value as text or compact JSON since 2.3.0, #63258, or
  error message plus suggestions, then `Warnings:` and `Logs:` sections); `execute_tool::format_execute_output(text)`
  re-indents it for display as clients do (`@opencode/util/execute-output`: the JSON result through `JSON.parse` and
  `JSON.stringify(_, null, 2)`, the sections apart; the TUI's execute dialog uses it); `execute_tool::output(...)` builds the
  `{ output, toolCalls, error?, files }` structure; `ProgressRows` tracks `{ tool, status, input? }` progress rows
  from the host hooks (fetch rows show `{ method, url }`); `display_input`, `fetch_display`, `output_file`.
- `catalog::inventory(&catalog, &namespaces, &pinned)` builds the catalog tree; `catalog::summarize(&inventory,
  None)` applies the 2 000-token inline budget (chars/4 rounded; every namespace line shown; pinned listings always
  shown; remaining listings round-robin per namespace in path order, a namespace stopping at its first listing that
  does not fit). The stored summary is `{total, namespaces: [{name, description?, tools: [every path], entries}]}`
  (2.2.7); a 2.2.5 summary does not decode, so core restates the whole catalog. `catalog::render(&summary)` produces
  the `core/codemode` instruction text; `catalog::update(&previous, &current, &lookup(&inventory))` the change delta
  (#62176): new namespaces and tools (new listings share a 1 000-token budget while the catalog is partial, the rest
  counted), the `search` note when the catalog becomes partial, changed listings the model saw, and removed tools and
  namespaces — or the whole catalog when that is shorter, or `None` when nothing the model was told changed.
  `catalog::{EMPTY_CATALOG, REMOVED_CATALOG}`.
- `experimental.codemode_vm` (#62358) was a flag for upstream's bytecode VM. Since 2.3.0 upstream always runs the VM
  and the key is deprecated and ignored; ocd accepts it and runs this interpreter, which is tested against the VM.

### OpenAPI (`openapi`)

`openapi::from_spec(options)` turns an OpenAPI 3.x document into `ToolEntry`s plus a `skipped` list;
`OpenApiHost` executes the generated tools over an `HttpClient` with host-resolved auth. See the module docs.

## Tests

`cargo test -p oc-codemode` (all green):

- `tests/upstream_replay.rs` — replays every program execution recorded from upstream's test suite and requires an
  identical `Result`. Two corpora, both generated by `tests/recorder/record.sh`, which instruments upstream (format
  in `tests/fixtures/upstream/README.md`):
  - `tests/fixtures/upstream/`: the 2.2.2 suite on the legacy interpreter (with 2.2.7's three changed results).
    1 798 of 1 802 recordings match; the 4 others are in `tests/fixtures/replay-skips.txt`: 2 permanent divergences
    and 2 results 2.3.0 changed (BM25 `search` no longer finds `get_timezones` for "zones", and calling a namespace
    names its tools), which the 2.3.0 corpus carries.
  - `tests/fixtures/upstream-2.3.0/`: the 2.3.0 suite on the bytecode VM, the only engine from 2.3.0. 1 841 of
    1 843 recordings match; the 2 others, in `tests/fixtures/replay-skips-2.3.0.txt`, are the same 2 permanent
    divergences (no 2.3.0 feature is left unported; a record listed with a batch tag such as `P5:` must still differ,
    so a batch that ports its feature deletes the line). No recorded program differs because of the engine: on the 1 859 programs both
    suites run, upstream's own results are identical except the namespace-call message, and so are the 3 340
    sampled test262 files. Replayed once against the full 2.3.0 test262 recording (13 330 files), ocd is identical
    except 9 files that write array indices near 2^32, beyond this crate's dense 10 000 000-element arrays (the
    sample's one documented divergence and 8 outside the sample, where 2.2 gave the same results).

  The recorder kit (`tests/recorder/`) now targets 2.3.0's `runtime/execute.ts`, `runtime/extensions.ts` and
  `tool-runtime.ts`. `OC_CODEMODE_CORPUS=2.3.0` points the tests that read one corpus (`fixtures()`, e.g.
  `tests/openapi.rs`) at the 2.3.0 recording.
- `tests/upstream_2_2.rs` — the cases upstream added in 2.2: promise jobs in V8 order (29 cases), stalled awaits,
  tool results settling like I/O, located parse errors, the nesting-depth failure, timeouts inside built-in work,
  promise-like `search` results, `for...in` over inherited keys, generator receiver checks, and extension `before`
  hooks that run before the host function.
- `tests/upstream_2_1.rs` — the cases upstream added up to 2.1.4: jobs run to completion, the `Uint8Array.from`
  mapper, non-coercing byte searches, `flatMap` holes, the host's `Number#toString` radix check, `fetch` refusing
  `file:` URLs, the fetch display method fallback, and recursion depth as 2.1.4 reports it.
- `tests/extensions_replay.rs` (44 recordings; 63 from 2.3.0, 5 of which a replay cannot rebuild, listed in
  `tests/fixtures/extension-skips-2.3.0.txt`) and `tests/hooks.rs` (16 recordings, both corpora) — extension host
  functions and call hooks, including hook ordering and denials.
- `tests/test262_sample.rs` — a deterministic quarter (3 340 files) of upstream's test262 conformance runs with a port
  of its harness, recorded from 2.2.2 and from 2.3.0 (whose harness defines `assert` in a one-line program prelude
  over the host's `$assert`); all identical except 1 documented in `tests/fixtures/test262-skips.txt` (and
  `test262-skips-2.3.0.txt`).
- `tests/parser_acorn.rs` — parser vs acorn 8.15 (with `allowImportExportEverywhere`) on 2 243 snippets and all
  15 008 vendored test262 files (AST and exact errors; regenerate with `node tests/fixtures/parser/gen.js`).
- `tests/upstream_2_3_0.rs` — where 2.3.0's VM differs from the 2.2 interpreter beyond upstream's own tests
  (`tests/fixtures/vm-2.3.0.json`, 80 programs run through upstream 2.3.0 by `tests/recorder/vm-cases.ts`). The
  VM's standard library is partly program code, so: built-ins that walk an argument with `for...of` (`new
  Map/Set/WeakMap/WeakSet`, `Object.groupBy`/`Map.groupBy`, `Object.fromEntries`, `new AggregateError`,
  `Math.sumPrecise`) fail with `InvalidDataValue` `TypeError: for...of requires an iterable value, received …`; a sort
  comparator's result is read as unary `+` reads it (`valueOf`/`toString` run and may throw, a promise is `Unary
  operators require data values.`); `String.replace` callback results and `Headers`/`URLSearchParams` pair items
  are converted with `String(…)` (an object's own `toString` runs); a Set-like operand with a negative size is a
  `RangeError`, a `keys()` result that is not an object is `Set.<method> expects 'keys' to return an iterator
  object.`; `GetIteratorDirect` on a primitive is `An iterator must be an object.`; well-known symbols print as
  `Symbol(codemode.iterator)`/`Symbol(codemode.async-iterator)`. Not followed (9 cases noted `gap`): the VM reads an
  iterator's `next` without checking it and fails only when calling it, with a `<call> is not a function.` naming the
  program call that made the iterator.
- `tests/upstream_2_2_7.rs` — the 2.2.6–2.2.7 module support: 80 programs (`import`, `import()`, `require`, aliases,
  caching, located failures, exports, `import.meta`, extension globals as modules) run through upstream 2.2.7's Code
  Mode (`tests/fixtures/modules-2.2.7.json`), upstream's new `errors.test.ts`/`extensions.test.ts` cases, and
  `scanTools` on module code. The two `errors.test.ts` recordings of the replay corpus whose behaviour 2.2.7 changed
  (`import x from "y"`, `export const x = 1`) carry 2.2.7's results; the two nested-import programs of
  `modules-2.2.7.json` carry 2.3.0's.
- `tests/signatures.rs` — every recorded tool signature, catalog read, schema-rendering call, `toolExpression`, and
  `searchSignature` against upstream (both corpora; 2.3.0's freeform tool signature waits for batch P1).
- `tests/search_ranking.rs` — `search` over a generated catalog of 400 tools (camelCase, kebab and snake names,
  plurals, nested namespaces with descriptions, parameter names): 272 programs (queries, namespace filters, paging,
  exact paths, unknown tools and namespace calls) with the results upstream 2.3.0 gives
  (`tests/fixtures/search-2.3.0.json`, regenerated by `tests/recorder/search-cases.ts`).
- `tests/catalog.rs` — catalog summarization, instruction render and 240 update texts against goldens produced by
  upstream core 2.2.7 (`tests/recorder/gen-catalog-golden.ts`).
- `tests/stdlib_*.rs` — per-module parity cases whose expectations were produced by upstream (array, collections,
  iterator, string/regexp, number/math, date incl. other time zones, url/headers/bytes/web); all but the date cases
  were re-checked against 2.2.2, and all of them against 2.3.0, with `tests/recorder/stdlib-check.ts`
  (`STDLIB_DUMP=<file>` dumps them). The 11 whose result 2.3.0's VM changed carry 2.3.0's result; 2 more keep 2.2's
  (the `gap` of `tests/upstream_2_3_0.rs`).
- `tests/openapi.rs` — every recorded `OpenAPI.fromSpec` call and HTTP round trip, plus 49 extra upstream-generated
  cases.
- `tests/fetch_stream.rs` — 46 programs reading streaming responses from a local server (readers, `for await`
  with a streaming `TextDecoder`, one-shot bodies, null bodies, lock and reader errors, `Headers`, the response as
  JSON, a redirect) with the `execute` outputs upstream core 2.3.0 gave against the same routes
  (`tests/fixtures/fetch-2.3.0.json`, `tests/recorder/fetch-cases.ts`), plus the bodies the server sees cancelled
  (left open at the program's end, a cancelled reader, a `break`, a redirect's body) and a pending request aborted
  when `execute` is interrupted.
- `tests/fetch.rs` (including the 2.2.7 fragment, credential and redirect rules), `tests/teardown.rs`, and unit
  tests in `number`, `tool_schema`, `catalog`, `execute_tool` (including upstream's `execute-output.test.ts`).

## Modules (upstream 2.2.6+, #61151)

Programs parse with acorn's `allowImportExportEverywhere` (still sloppy-mode scripts). A module is a global, by its Node
module name: `node:` is stripped, a global of that name is used (built-ins, bindings and extension globals), else the
aliases `fs/promises` (`fs.promises`), `url` (`{URL, URLSearchParams}`), `util` (`{TextEncoder, TextDecoder}`) and
`buffer` (`{Buffer, atob, btoa}`, only the globals that exist). Anything else fails with ``Error: Cannot import
"<name>": it isn't available in Code Mode, which can't load packages or files. Use the provided globals, or call tools
through `tools`.``

- Top-level import declarations bind immutably before any code runs (before hoisting): a default import binds the
  module, `* as ns` a null-prototype namespace of the module's own enumerable properties plus `default`, a named import
  one of the namespace's properties or `SyntaxError: "<spec>" has no export named "<name>".` at the binding. An import
  nested in the program's own blocks (`if`, loops, labels, `try`, `switch`) does nothing: it binds and loads nothing,
  as on 2.3.0's VM. One reached inside a function fails with `SyntaxError: An import declaration may only appear at
  the top level of the program.`
- `import(x)` (options evaluated and ignored) returns a promise of the namespace, rejected when the module fails to load.
- `require(name)` (global, after `search`) returns the module; a non-string fails with `TypeError: require expects a
  module name string.`
- Modules and namespaces are cached per execution, so repeated imports see the same object.
- A top-level `export` is a `ParseError`: `SyntaxError: Exports aren't available. Return the result instead.`; a nested
  one is `UnsupportedSyntax` when reached. `import.meta` parses and is `UnsupportedSyntax` (`MetaProperty`).
- Upstream's object-valued extension globals (#61151 lets `Extension.make` take objects, e.g. an `fs` object whose
  functions fire the hooks) are not part of this crate's host API, which only takes functions; core never uses them.

## Unsupported JavaScript (same as upstream, same diagnostics)

Classes and private fields, user-defined constructors (`new f()` on a program function), getters/setters in object
literals, BigInt, arbitrary `Symbol()` values and symbol-keyed properties (only `Symbol.iterator`/`asyncIterator`),
exports and `import.meta`, `new.target`, `super`, `with`, `debugger`, `eval`/`Function`, timers (`setTimeout`,
`queueMicrotask`), `Proxy`/`Reflect`, `Object.defineProperty`/property descriptors, `for...in` skipping keys deleted during the loop,
`ArrayBuffer`/`DataView`/typed arrays other than `Uint8Array`, `Request`/`Response`/`Blob`, `crypto.subtle`, non-UTF-8
`TextDecoder`, `Iterator.concat`/`zip`, array methods on array-likes (`Array.prototype.slice.call(arguments)`),
TypeScript syntax. Unsupported syntax is reported lazily (when reached) as `UnsupportedSyntax`.

### Schedule checks

- `scan::scan_tools(code)` is `CodeMode.scanTools`: the `tools.<path>` a program calls statically, in first-seen
  order, or the diagnostic for dynamic bracket access, aliasing/passing `tools`, a syntax error or empty code
  (cases captured from upstream in `tests/fixtures/scan_tools.jsonl`).
- `CodeMode::with_bindings(vec![(name, json)])` binds read-only JSON globals (upstream `execute(code, bindings)`),
  which top-level declarations may shadow.
- `parser::validate_regexp(pattern, flags)` exposes acorn's regular expression validation.

## Known differences from upstream

- A timeout that fires while a tool's uninterruptible cleanup is still running upstream (Effect waits for it) cannot
  occur here: host futures are dropped immediately, so no `TimeoutExceeded` warning is produced in that case.
- Strings with lone surrogates become U+FFFD when they cross to the host as JSON (Rust strings are UTF-8); inside the
  program, including `JSON.stringify`, they are preserved.
- Arrays are dense: an index write past 10 000 000 elements throws `RangeError: Invalid array length` instead of
  creating a sparse array.
- Exact JS stack-depth overflows of the host engine are approximated: recursion, async recursion included, stops at
  upstream's 10 000-call limit with a located `RangeError`, and source nested deeper than the parser's limit (10 000
  levels, member/call chain links included) fails with upstream's "Execution exceeded the maximum nesting depth."
  before running, where upstream fails when its evaluation overflows Bun's stack.
- Upstream's runner yields to the host every millisecond of a long job (so other Effect fibers keep running); here
  each execution has its own thread, so there is nothing to yield to. A synchronous generator body that awaits
  across its caller (only possible through `search(...)` arguments holding promises inside a generator) is not
  supported.
- `scan_tools` walks the parser's ESTree JSON, where node kinds outside the interpreter's subset (classes,
  `new.target`, `import.meta`) carry no children, so a `tools.<path>` call inside a class body is not found (upstream finds it; the
  program fails when it runs either way).
