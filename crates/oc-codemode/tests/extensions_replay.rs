//! Replays recorded executions that call extension globals (upstream extensions.test.ts and friends): each host
//! function returns exactly what the recorded upstream host function returned, and results must match.

mod common;

use common::*;
use oc_codemode::codemode::CodeMode;

/// The corpus ocd is held to in full (upstream 2.2.2's suite).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_executions_replay_exactly() {
    replay_extensions_of(corpus(""), "").await;
}

/// opencode 2.3.0's suite on its bytecode VM; `extension-skips-2.3.0.txt` lists the records a replay cannot reproduce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_executions_replay_2_3_0() {
    replay_extensions_of(corpus("2.3.0"), "extension-skips-2.3.0.txt").await;
}

/// Known divergences: `file<TAB>seq<TAB>reason` lines.
fn skips(name: &str) -> Vec<(String, u64)> {
    if name.is_empty() {
        return Vec::new();
    }
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            Some((parts.next()?.to_string(), parts.next()?.parse().ok()?))
        })
        .collect()
}

async fn replay_extensions_of(dir: std::path::PathBuf, skip_file: &str) {
    let skips = skips(skip_file);
    let mut total = 0;
    let mut skipped = 0;
    let mut failures = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for path in files {
        if !path.to_string_lossy().ends_with(".test.jsonl") {
            continue;
        }
        for record in read_jsonl(&path) {
            let Some(extensions) = record["extensions"].as_array() else { continue };
            if extensions.is_empty()
                || record["via"] != "codemode"
                || record.get("result").is_none()
                || !record["hooks"].as_array().unwrap().is_empty()
            {
                continue;
            }
            total += 1;
            let exts = common::ext::replay_extensions(&record);
            let runtime = match CodeMode::new(tree_entries(&record["tree"]), host_arc(&record), exts, limits_of(&record)) {
                Ok(r) => r.with_bindings(bindings_of(&record)),
                Err(e) => {
                    failures.push(format!("{}: make failed {e}", record["test"]));
                    continue;
                }
            };
            let ours = runtime.execute(record["code"].as_str().unwrap()).await.to_json();
            let expected = decode(&record["result"]).unwrap();
            if !json_eq(&ours, &expected) {
                let key = (record["file"].as_str().unwrap_or("").to_string(), record["seq"].as_u64().unwrap_or(0));
                if skips.contains(&key) {
                    skipped += 1;
                    continue;
                }
                failures.push(format!(
                    "{} {} (seq {})\n  code: {}\n  expected {expected}\n  got      {ours}",
                    record["file"], record["test"], record["seq"], record["code"]
                ));
            }
        }
    }
    eprintln!("extension replays: {total}, {skipped} known divergences, {} failed", failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
