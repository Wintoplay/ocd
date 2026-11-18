//! Replays every program execution recorded from the upstream test suite and compares results exactly.
//!
//! Records that cannot be replayed generically (extension host functions, OpenAPI tools, interrupted runs) are
//! covered by dedicated tests. Two corpora are replayed: `tests/fixtures/upstream/` (upstream 2.2.2's suite with
//! 2.2.7's changes) and `tests/fixtures/upstream-2.3.0/` (2.3.0's suite on its bytecode VM). Known, documented
//! divergences are listed in `tests/fixtures/replay-skips.txt` and `replay-skips-2.3.0.txt`
//! (`file<TAB>seq<TAB>reason`).

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use oc_codemode::codemode::CodeMode;
use serde_json::Value as Json;

/// A corpus's known divergences (`file<TAB>seq<TAB>reason`). A reason that starts with a batch tag (`P5:`, `P6:`)
/// marks a 2.3.0 feature ocd has not ported yet: the record must still differ, so the entry is removed when the batch
/// lands. Untagged reasons are permanent.
fn skips(name: &str) -> HashMap<(String, u64), String> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            Some(((parts.next()?.to_string(), parts.next()?.parse().ok()?), parts.next().unwrap_or("").to_string()))
        })
        .collect()
}

fn pending(reason: &str) -> bool {
    let tag = reason.split(':').next().unwrap_or("");
    tag.len() > 1 && tag.starts_with('P') && tag[1..].chars().all(|c| c.is_ascii_digit())
}

pub fn replayable(record: &Json) -> bool {
    record.get("result").is_some()
        && record["via"] == "codemode"
        && record.get("openapi").is_none()
        && record.get("truncated").is_none()
        && record["extensions"].as_array().map(|a| a.is_empty()).unwrap_or(true)
        && record["hooks"].as_array().map(|a| a.is_empty()).unwrap_or(true)
}

/// Runs one record; returns the mismatch description if any.
pub async fn replay(record: &Json) -> Option<String> {
    let entries = tree_entries(&record["tree"]);
    let host = host_arc(record);
    let runtime = match CodeMode::new(entries, host.clone(), Vec::new(), limits_of(record)) {
        Ok(r) => r.with_bindings(bindings_of(record)),
        Err(e) => return Some(format!("make failed: {e}")),
    };
    let code = record["code"].as_str().unwrap_or("");
    let ours = match tokio::time::timeout(std::time::Duration::from_secs(20), runtime.execute(code)).await {
        Ok(result) => result.to_json(),
        Err(_) => return Some("replay timed out".into()),
    };
    let expected = decode(&record["result"]).unwrap_or(Json::Null);
    let crossed = host.crossing_mismatches();
    if json_eq(&ours, &expected) && crossed.is_empty() {
        None
    } else {
        Some(format!("expected {expected}\n     got {ours}{}", crossed.join("")))
    }
}

/// The transcendental `Math` functions come from the platform's libm, in JavaScriptCore as in ocd, so their last
/// digits depend on the platform. The recordings come from Bun on Linux x86_64 (glibc); elsewhere a mismatch in a
/// program that calls them counts as a known divergence, since upstream there prints that platform's digits too.
fn libm_dependent(code: &str) -> bool {
    const LIBM: &[&str] = &[
        "sin", "cos", "tan", "asin", "acos", "atan", "atan2", "sinh", "cosh", "tanh", "asinh", "acosh", "atanh", "log",
        "log2", "log10", "log1p", "exp", "expm1", "cbrt", "pow", "hypot",
    ];
    !cfg!(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))
        && LIBM.iter().any(|f| code.contains(&format!("Math.{f}(")) || code.contains(&format!("\"{f}\"")) || code.contains(&format!("'{f}'")))
}

/// Replays every replayable record of one corpus against its divergence list; panics on any unlisted mismatch and on
/// any batch-tagged entry that now matches.
async fn replay_corpus(dir: std::path::PathBuf, skip_file: &str) {
    let skips = skips(skip_file);
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".test.jsonl"))
        .collect();
    files.sort();
    let mut total = 0;
    let mut passed = 0;
    let mut skipped = 0;
    let mut failures: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let only = std::env::var("REPLAY_ONLY").ok();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if let Some(only) = &only
            && !name.contains(only.as_str())
        {
            continue;
        }
        for record in read_jsonl(&path) {
            if !replayable(&record) {
                continue;
            }
            total += 1;
            let file = record["file"].as_str().unwrap_or("").to_string();
            let seq = record["seq"].as_u64().unwrap_or(0);
            let outcome = replay(&record).await;
            let listed = skips.get(&(file.clone(), seq));
            match outcome {
                None => {
                    passed += 1;
                    if let Some(reason) = listed
                        && pending(reason)
                    {
                        stale.push(format!("{file}\t{seq}\t{reason}"));
                    }
                }
                Some(_) if listed.is_some() => skipped += 1,
                Some(_) if libm_dependent(record["code"].as_str().unwrap_or("")) => skipped += 1,
                Some(diff) => failures.push(format!(
                    "{file}\t{seq}\t{}\n  code: {}\n  {diff}",
                    record["test"].as_str().unwrap_or(""),
                    record["code"].as_str().unwrap_or("").replace('\n', "\n        ")
                )),
            }
        }
    }
    eprintln!(
        "{}: replayed {total}: {passed} passed, {skipped} known divergences, {} failed",
        dir.file_name().unwrap().to_string_lossy(),
        failures.len()
    );
    assert!(
        stale.is_empty(),
        "these {skip_file} entries now replay exactly; remove them:\n{}",
        stale.join("\n")
    );
    if !failures.is_empty() {
        let report = failures.join("\n\n");
        let out = std::env::temp_dir().join(format!("oc-codemode-replay-failures-{skip_file}"));
        std::fs::write(&out, &report).ok();
        let shown: Vec<&String> = failures.iter().take(20).collect();
        panic!(
            "{} replay mismatches (full list in {}):\n\n{}",
            failures.len(),
            out.display(),
            shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n")
        );
    }
    let _ = Arc::new(());
}

/// The corpus ocd is held to in full: upstream 2.2.2's suite, with 2.2.7's two changed results.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_executions_replay_exactly() {
    replay_corpus(corpus(""), "replay-skips.txt").await;
}

/// opencode 2.3.0's suite, recorded on its bytecode VM. The records whose result 2.3.1 changed are listed in
/// `replay-skips-2.3.0.txt` with the 2.3.1 record that replaces them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_2_3_0_executions_replay() {
    replay_corpus(corpus("2.3.0"), "replay-skips-2.3.0.txt").await;
}

/// opencode 2.3.1's suite, after the language/Code Mode split (#64487); `replay-skips-2.3.1.txt` lists its known
/// divergences.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_2_3_1_executions_replay() {
    replay_corpus(corpus("2.3.1"), "replay-skips-2.3.1.txt").await;
}
