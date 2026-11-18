//! What opencode 2.3.1 changed in Code Mode beyond its own test suite: Node's `Buffer` global (#64365), tool and
//! extension arguments crossing as host copies (functions left out, promises awaited, a program's `toJSON` not run,
//! iterators, symbols and namespace references refused; #64487, #64513), `search("text")` (#64347), and the messages
//! the language/Code Mode split reworded.
//!
//! `tests/fixtures/cases-2.3.1.json` holds the programs with the results upstream 2.3.1 gives
//! (`tests/recorder/cases-2.3.1.ts` regenerates them), all run in one runtime: the tools `echo` (returns its input)
//! and `shop.show` (returns nothing), and the extension `host` with `echo` (returns its arguments), `echoAsync` (the
//! same, asynchronously) and `bytes` (a host Buffer of "host"). Every result must be identical, except the cases
//! with a `gap` note, which must still differ: when one starts to match, delete its note.

mod common;

use std::sync::Arc;

use futures::future::BoxFuture;
use common::json_eq;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::{Host, HostCall, HostFunction, HostValue, SimpleExtension, ToolCallError};
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};
use serde_json::{json, Value as Json};

struct Echo;

impl Host for Echo {
    fn call(&self, _path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        Box::pin(async move { Ok(input) })
    }
}

struct Function {
    name: &'static str,
    asynchronous: bool,
    reply: fn(Vec<HostValue>) -> HostValue,
}

impl HostFunction for Function {
    fn name(&self) -> &str {
        self.name
    }
    fn length(&self) -> usize {
        0
    }
    fn asynchronous(&self) -> bool {
        self.asynchronous
    }
    fn call(&self, args: Vec<HostValue>) -> HostCall {
        let value = (self.reply)(args);
        if self.asynchronous {
            HostCall::Pending(Box::pin(async move { Ok(value) }))
        } else {
            HostCall::Ready(Ok(value))
        }
    }
}

fn runtime() -> CodeMode {
    let globals: Vec<(String, Arc<dyn HostFunction>)> = vec![
        ("echo".into(), Arc::new(Function { name: "echo", asynchronous: false, reply: HostValue::Array })),
        ("echoAsync".into(), Arc::new(Function { name: "echoAsync", asynchronous: true, reply: HostValue::Array })),
        ("bytes".into(), Arc::new(Function { name: "bytes", asynchronous: false, reply: |_| HostValue::Buffer(b"host".to_vec()) })),
    ];
    CodeMode::new(
        vec![
            ToolEntry::Tool(ToolDef::new("echo", "Echo the input", json!({})).with_output(json!({}))),
            ToolEntry::Tool(ToolDef::new("shop.show", "Show raw input", json!({ "type": "object" }))),
        ],
        Arc::new(Echo),
        vec![Arc::new(SimpleExtension { name: "host".into(), globals })],
        ExecutionLimits { timeout_ms: Some(5000.0), ..Default::default() },
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn results_match_upstream_2_3_1() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cases-2.3.1.json");
    let cases: Vec<Json> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let runtime = runtime();
    let only = std::env::var("CASES_ONLY").ok();
    let (mut failures, mut stale) = (Vec::new(), Vec::new());
    for case in &cases {
        let code = case["code"].as_str().unwrap();
        if let Some(only) = &only
            && !code.contains(only.as_str())
        {
            continue;
        }
        let ours = runtime.execute(code).await.to_json();
        let same = json_eq(&ours, &case["result"]);
        match (same, case.get("gap")) {
            (false, None) => failures.push(format!("{code}\n  expected {}\n  got      {ours}", case["result"])),
            (true, Some(_)) => stale.push(code.to_string()),
            _ => {}
        }
    }
    let report = failures.join("\n\n");
    std::fs::write(std::env::temp_dir().join("oc-codemode-cases-2.3.1.txt"), &report).ok();
    assert!(stale.is_empty(), "these cases now match; delete their gap notes:\n{}", stale.join("\n\n"));
    assert!(failures.is_empty(), "{} of {} differ:\n\n{report}", failures.len(), cases.len());
}
