# Upstream CodeMode fixtures, opencode 2.3.0

The same kind of recording as `../upstream/` (format: `../upstream/README.md`), made from opencode **v2.3.0**
(`packages/codemode` at commit `a6b15d33da`), where the bytecode VM is the only engine, with Bun 1.4.4. The test262
shards keep the same deterministic quarter of the files (3 340 of 13 330 executions; the vendored corpus is the same
15 008 files at test262 `250f204f`, 1 678 of them skipped by upstream's `skipped.txt`).

Regenerate with the recording kit in `tests/recorder/` (it targets v2.3.0's `runtime/execute.ts`,
`runtime/extensions.ts` and `tool-runtime.ts`):

```sh
tests/recorder/record.sh OPENCODE_2_3_0 CORPUS WORK
python3 -I tests/recorder/install.py WORK/out tests/fixtures/upstream-2.3.0
```

The instrumented suite gives upstream's own result except for two tests that check upstream's sources or timing,
not behaviour: `state.test.ts` "the VM does not import Effect" (the recording kit imports Effect into `src/`) and
`limits.test.ts` "reporting many unhandled rejections still times out" (its recorded result is the
`TimeoutExceeded` upstream gives, but the recorder's bookkeeping makes the test outlast bun's 5 s test timeout). The test262 file
`language/global-code/import.js` fails upstream too (imports became module loads in 2.2.7).

Additions to the format, all optional, so the 2.2 recordings read the same way:

| Field | Description |
| --- | --- |
| `bindings` | on an execution: the program's `runtime.execute(code, bindings)` values, when given |
| `via` | also `"program"` (`runtime.program().run()`) and `"resume"` (`runtime.resume().run()`); the replay tests skip both, which are suspend/resume library APIs ocd does not implement. `"executeProgram"` is now `startProgram` called directly (the test262 harness) |
| `suspended` | `true` instead of `result` when a `program` run ended suspended |
| tool `freeform` | `true` on a freeform tool (#61703): its signature is `tools.x(/** desc */ name: string)` |
| tool `mode` | `"CodeModeIdempotent"` or `"CodeModeDurable"` for tools made with `Tool.idempotent`/`Tool.durable` (their `start`/`wait` calls are not recorded) |
| test262 `code` | `"use strict"; ` + the harness prelude (one line defining `assert` over the host's `$assert`) + `\n` + the file source (+ the async drain) |

Encoding changes that only affect hostile host values: a function whose `name` is not a plain string property is
`{"$function": null}`, an object's text is never computed by calling its own `toString`, and holes count against the
200 000-node budget (a 2^32 - 1 long sparse array is cut with `{"$tooLarge": true}`).

Hook events are recorded when the hook's Effect runs: the VM's extension bridge builds the `extension.before` Effect
at each step of a synchronous extension call, but runs it once.

## How the tests use it

`tests/upstream_replay.rs`, `tests/test262_sample.rs`, `tests/extensions_replay.rs`, `tests/hooks.rs` and
`tests/signatures.rs` replay this corpus next to `../upstream/`. Records ocd does not reproduce are listed in
`../replay-skips-2.3.0.txt` (a `P5:`/`P6:` tag marks a 2.3.0 feature not ported yet; the replay test fails when such a
record starts to match, so the batch that ports it deletes the line), `../extension-skips-2.3.0.txt` (values a
replay cannot rebuild) and `../test262-skips-2.3.0.txt`. Freeform tool signatures are left to batch P1.
`OC_CODEMODE_CORPUS=2.3.0` points every other test that reads `fixtures()` (e.g. `tests/openapi.rs`) at this corpus.
