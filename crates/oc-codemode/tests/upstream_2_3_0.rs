//! Where opencode 2.3.0's bytecode VM, its only Code Mode engine, behaves differently from the 2.2 interpreter on
//! programs its own test suite does not run. Much of the VM's standard library is program code (its prelude), so
//! built-ins that walk an argument with `for...of` fail with for...of's error, a sort comparator's result is read as
//! unary `+` reads it, and `String(...)` conversions run an object's own `toString`. An import nested in the program's
//! blocks binds nothing instead of failing.
//!
//! `tests/fixtures/vm-2.3.0.json` holds these programs with the results upstream 2.3.0 gives
//! (`tests/recorder/vm-cases.ts` regenerates them). Every result must be identical, except the cases with a `gap`
//! note, which must still differ: when one starts to match, delete its note.

mod common;

use std::sync::Arc;

use common::json_eq;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::host::NoTools;
use serde_json::Value as Json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vm_results_match_upstream() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vm-2.3.0.json");
    let cases: Vec<Json> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let runtime = CodeMode::new(
        Vec::new(),
        Arc::new(NoTools),
        Vec::new(),
        ExecutionLimits { timeout_ms: Some(5000.0), ..Default::default() },
    )
    .unwrap();
    let (mut failures, mut stale) = (Vec::new(), Vec::new());
    for case in &cases {
        let code = case["code"].as_str().unwrap();
        let ours = runtime.execute(code).await.to_json();
        let same = json_eq(&ours, &case["result"]);
        match (same, case.get("gap")) {
            (false, None) => failures.push(format!("{code}\n  expected {}\n  got      {ours}", case["result"])),
            (true, Some(_)) => stale.push(code.to_string()),
            _ => {}
        }
    }
    assert!(stale.is_empty(), "these cases now match; delete their gap notes:\n{}", stale.join("\n\n"));
    assert!(failures.is_empty(), "{} of {} differ:\n\n{}", failures.len(), cases.len(), failures.join("\n\n"));
}
