//! `search(...)` ranking (name-weighted BM25) and the "Did you mean" suggestions of unknown tools and namespace calls,
//! over a generated catalog of 400 tools, against opencode 2.3.0. `tests/fixtures/search-2.3.0.json` holds the
//! catalog and upstream's result for every program (`tests/recorder/search-cases.ts` regenerates it).

mod common;

use std::sync::Arc;

use common::json_eq;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};
use serde_json::{json, Value as Json};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_ranking_matches_upstream() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/search-2.3.0.json");
    let fixture: Json = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut entries = Vec::new();
    for ns in fixture["namespaces"].as_array().unwrap() {
        entries.push(ToolEntry::Namespace {
            path: ns["path"].as_str().unwrap().to_string(),
            description: ns["description"].as_str().map(String::from),
        });
    }
    for tool in fixture["catalog"].as_array().unwrap() {
        let params: Vec<&str> = tool["params"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
        let properties: serde_json::Map<String, Json> =
            params.iter().map(|p| (p.to_string(), json!({ "type": "string" }))).collect();
        let input = json!({ "type": "object", "properties": properties, "required": params });
        entries.push(ToolEntry::Tool(
            ToolDef::new(tool["path"].as_str().unwrap(), tool["description"].as_str().unwrap(), input)
                .with_output(json!({ "type": "string" })),
        ));
    }
    let runtime = CodeMode::new(
        entries,
        Arc::new(NoTools),
        Vec::new(),
        ExecutionLimits { timeout_ms: Some(5000.0), ..Default::default() },
    )
    .unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    let mut failures = Vec::new();
    for case in cases {
        let code = case["code"].as_str().unwrap();
        let ours = runtime.execute(code).await.to_json();
        if !json_eq(&ours, &case["result"]) {
            failures.push(format!("{code}\n  expected {}\n  got      {ours}", case["result"]));
        }
    }
    assert!(cases.len() > 250);
    assert!(failures.is_empty(), "{} of {} differ:\n\n{}", failures.len(), cases.len(), failures.join("\n\n"));
}
