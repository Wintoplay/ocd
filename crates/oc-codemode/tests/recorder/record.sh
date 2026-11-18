#!/usr/bin/env bash
# Regenerates tests/fixtures/upstream from upstream's codemode test suite (see tests/fixtures/upstream/README.md).
#
#   record.sh UPSTREAM CORPUS WORK [OUT]
#
# UPSTREAM  an opencode checkout at the tag being tracked (v2.3.1), with `bun install` done (node_modules present)
# CORPUS    a directory holding the vendored test262 `built-ins/` and `language/` trees (upstream's
#           `bun run script/sync-test262.ts` output, at the revision in test/test262/manifest.json)
# WORK      an empty scratch directory for the instrumented package copy and the logs
# OUT       where the full recording goes (default: WORK/out); install it with install.py
# BUN       the bun binary (default: bun on PATH)
#
# The instrumented copy is upstream's packages/codemode plus this directory's files: src/codemode/record.ts (all
# encoding and output, inert unless CODEMODE_RECORD_DIR is set), src/codemode/tool-schema.ts (a recording shim over the
# original, renamed src/codemode/tool-schema-impl.ts), record-preload.ts (tracks the current test),
# script/record-extras.ts (search-signature.json), and hooks.patch (the recording calls in codemode/codemode.ts,
# codemode/tool-runtime.ts, codemode/openapi/{index,runtime}.ts, language/runtime.ts and
# language/runtime/{execute,extensions}.ts). The patch targets the language/Code Mode layout of v2.3.1 (#64487); the
# v2.3.0 recording came from the same kit before that split (src/runtime/*), and the v2.2.x recordings from the legacy
# interpreter (src/interpreter/*) and an older kit. Instrumented, test/language-boundary.test.ts fails (the language
# layer imports the recorder), like state.test.ts's "does not import Effect".
set -euo pipefail

KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UPSTREAM="$(cd "$1" && pwd)"
CORPUS="$(cd "$2" && pwd)"
WORK="$(mkdir -p "$3" && cd "$3" && pwd)"
OUT="${4:-$WORK/out}"
BUN="${BUN:-bun}"

PKG="$WORK/packages/codemode-rec"
rm -rf "$WORK/packages"
mkdir -p "$WORK/packages"
ln -sfn "$UPSTREAM/node_modules" "$WORK/node_modules"
cp -r "$UPSTREAM/packages/codemode" "$PKG"
rm -rf "$PKG/test/test262/built-ins" "$PKG/test/test262/language"
mv "$PKG/src/codemode/tool-schema.ts" "$PKG/src/codemode/tool-schema-impl.ts"
cp "$KIT/record.ts" "$KIT/tool-schema.ts" "$PKG/src/codemode/"
cp "$KIT/record-preload.ts" "$PKG/"
cp "$KIT/record-extras.ts" "$PKG/script/"
(cd "$PKG" && patch -p1 --quiet <"$KIT/hooks.patch")

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
find "$OUT" -mindepth 1 -maxdepth 1 ! -name README.md -exec rm -rf {} +
LOGS="$WORK/logs"
rm -rf "$LOGS"
mkdir -p "$LOGS"
summary() { grep -E '^ *[0-9]+ (pass|fail|skip|todo|error)|^Ran |^\(fail\)' "$1" || true; }

# Pass 1: the regular suite, without the test262 corpus (test/test262.test.ts then defines no tests).
echo "pass 1: bun test (recording) -> $OUT"
# test/preload.ts is upstream's own preload (bunfig.toml); it is named again so both run.
PRELOAD=(--preload ./test/preload.ts --preload ./record-preload.ts)
(cd "$PKG" && CODEMODE_RECORD_DIR="$OUT" "$BUN" test "${PRELOAD[@]}" >"$LOGS/pass1.log" 2>&1) || true
summary "$LOGS/pass1.log"

# Pass 2: the vendored test262 corpus.
echo "pass 2: test262 (recording) from $CORPUS"
cp -r "$CORPUS/built-ins" "$CORPUS/language" "$PKG/test/test262/"
(cd "$PKG" && CODEMODE_RECORD_DIR="$OUT" "$BUN" test "${PRELOAD[@]}" test/test262.test.ts \
  >"$LOGS/pass2.log" 2>&1) || true
rm -rf "$PKG/test/test262/built-ins" "$PKG/test/test262/language"
summary "$LOGS/pass2.log"

(cd "$PKG" && "$BUN" run script/record-extras.ts "$OUT")
echo "logs: $LOGS"
echo "records per file:"
(cd "$OUT" && find . -name '*.jsonl' | sed 's#^\./##' | sort | xargs wc -l | sed '$d')
