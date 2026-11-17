//! session-runner-message.test.ts (opencode 2.3.0, #60674): a failed or interrupted assistant message is replayed
//! cut at its last local call that ran. Calls cut off before they ran are dropped (input errors stay), reasoning is
//! replayed only once the model moved on to output, and the tail block's provider state is never replayed.

use oc_core::lower::to_llm_messages;
use oc_schema::types::ModelRef;
use oc_schema::Message as SessionMessage;
use serde_json::{json, Value};

fn model(id: &str) -> ModelRef {
    ModelRef { id: id.into(), provider_id: "provider".into(), variant: None }
}

/// An assistant message of `model` (provider `provider`) with optional `finish` and `error`.
fn assistant(id: &str, model: &str, content: Value, finish: Option<&str>, error: Option<Value>) -> SessionMessage {
    let mut m = json!({
        "type": "assistant",
        "id": id,
        "time": {"created": 1, "completed": 1},
        "agent": "build",
        "model": {"id": model, "providerID": "provider"},
        "content": content,
    });
    if let Some(f) = finish {
        m["finish"] = json!(f);
    }
    if let Some(e) = error {
        m["error"] = e;
    }
    serde_json::from_value(m).expect("assistant")
}

fn lower(messages: &[SessionMessage], model_id: &str) -> Vec<Value> {
    to_llm_messages(messages, &model(model_id), Some("provider"))
        .into_iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect()
}

/// `[role, [part type or tool id]]` per message.
fn shape(messages: &[Value]) -> Vec<(String, Vec<String>)> {
    messages
        .iter()
        .map(|m| {
            let parts = m["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["type"].as_str().unwrap().to_string())
                .collect();
            (m["role"].as_str().unwrap().to_string(), parts)
        })
        .collect()
}

fn stop_aborted() -> Value {
    json!({"type": "aborted", "message": "Step interrupted"})
}

fn stream_dropped() -> Value {
    json!({"type": "provider.unknown", "message": "Stream dropped"})
}

fn read_ran(id: &str) -> Value {
    json!({"type": "tool", "id": id, "name": "read",
        "state": {"status": "completed", "input": {"path": "README.md"}, "content": [{"type": "text", "text": "Hello"}]},
        "time": {"created": 1, "completed": 1, "ran": 1}})
}

#[test]
fn retries_a_failed_stream_from_its_last_local_call_that_ran() {
    let failed = assistant(
        "msg_failed",
        "model",
        json!([
            {"type": "reasoning", "text": "Finished thought", "native": {"itemId": "rs_failed", "reasoningEncryptedContent": null}},
            {"type": "tool", "id": "hosted-completed", "name": "web_search", "executed": true, "native": {"itemId": "hosted_completed"},
                "state": {"status": "completed", "input": {"query": "Effect"}, "content": [{"type": "text", "text": "{\"found\":true}"}]},
                "time": {"created": 1, "completed": 1, "ran": 1}},
            {"type": "tool", "id": "hosted-failed", "name": "web_search", "executed": true, "native": {"itemId": "hosted_failed"},
                "state": {"status": "error", "input": {"query": "Effect"}, "error": {"type": "unknown", "message": "Step interrupted"}},
                "time": {"created": 1, "completed": 1, "ran": 1}},
            read_ran("local-ran"),
            {"type": "tool", "id": "hosted-after-local", "name": "web_search", "executed": true,
                "state": {"status": "error", "input": {"query": "Later"}, "error": {"type": "tool.result-missing", "message": "Provider did not return a tool result"}},
                "time": {"created": 1, "completed": 1, "ran": 1}},
        ]),
        Some("error"),
        Some(json!({"type": "unknown", "message": "Step interrupted"})),
    );
    // Hosted calls alone are provider-side reads the retry can redo, so the request never ends on an assistant reply.
    let hosted_only = assistant(
        "msg_hosted_only",
        "model",
        json!([
            {"type": "tool", "id": "hosted-only", "name": "web_search", "executed": true,
                "state": {"status": "completed", "input": {"query": "Effect"}, "content": [{"type": "text", "text": "{\"found\":true}"}]},
                "time": {"created": 1, "completed": 1, "ran": 1}},
        ]),
        Some("error"),
        Some(stream_dropped()),
    );
    let out = lower(&[failed, hosted_only], "model");
    assert_eq!(out.iter().map(|m| m["role"].as_str().unwrap()).collect::<Vec<_>>(), ["assistant", "tool"]);
    assert_eq!(out[1]["content"][0]["type"], "tool-result");
    assert_eq!(out[1]["content"][0]["id"], "local-ran");
    let content = out[0]["content"].as_array().unwrap();
    let ids: Vec<String> =
        content.iter().map(|p| p["id"].as_str().map(str::to_string).unwrap_or_else(|| p["type"].as_str().unwrap().to_string())).collect();
    assert_eq!(ids, ["reasoning", "hosted-completed", "hosted-completed", "hosted-failed", "hosted-failed", "local-ran"]);
    assert_eq!(content[0], json!({"type": "reasoning", "text": "Finished thought", "providerMetadata": {"provider": {"itemId": "rs_failed", "reasoningEncryptedContent": null}}}));
    assert_eq!(content[1]["providerMetadata"], json!({"provider": {"itemId": "hosted_completed"}}));
    assert_eq!(content[5], json!({"type": "tool-call", "id": "local-ran", "name": "read", "input": {"path": "README.md"}}));
}

#[test]
fn omits_unfinished_reasoning_from_an_interrupted_assistant_message() {
    let m = assistant(
        "msg_interrupted",
        "model",
        json!([
            {"type": "reasoning", "text": "Completed summary part", "native": {"itemId": "rs_interrupted", "reasoningEncryptedContent": null}, "time": {"created": 1, "completed": 1}},
            {"type": "reasoning", "text": "", "native": {"itemId": "rs_interrupted", "reasoningEncryptedContent": null}, "time": {"created": 1}},
        ]),
        None,
        None,
    );
    assert!(lower(&[m], "model").is_empty());
}

#[test]
fn drops_assistant_text_provider_state_across_model_changes_aborts_and_provider_failures() {
    let switched = assistant(
        "msg_switched",
        "old",
        json!([{"type": "text", "text": "Checking.", "native": {"phase": "commentary"}}]),
        Some("stop"),
        None,
    );
    let out = lower(&[switched], "new");
    assert_eq!(out[0]["content"], json!([{"type": "text", "text": "Checking."}]));

    let aborted = vec![
        assistant(
            "msg_aborted",
            "same",
            json!([
                {"type": "reasoning", "text": "Finished thought", "native": {"signature": "sig_kept"}, "time": {"created": 1, "completed": 1}},
                {"type": "text", "text": "Closed commentary.", "native": {"phase": "commentary"}},
                {"type": "tool", "id": "uncalled-stream", "name": "write",
                    "state": {"status": "error", "input": {}, "error": stop_aborted()}, "time": {"created": 1, "completed": 1}},
            ]),
            None,
            Some(stop_aborted()),
        ),
        assistant(
            "msg_aborted_mid_text",
            "same",
            json!([{"type": "text", "text": "Cut off sentence", "native": {"phase": "final_answer"}}]),
            None,
            Some(stop_aborted()),
        ),
    ];
    let out = lower(&aborted, "same");
    assert_eq!(
        out.iter().map(|m| m["content"].clone()).collect::<Vec<_>>(),
        vec![
            json!([
                {"type": "reasoning", "text": "Finished thought", "providerMetadata": {"provider": {"signature": "sig_kept"}}},
                {"type": "text", "text": "Closed commentary.", "providerMetadata": {"provider": {"phase": "commentary"}}},
            ]),
            json!([{"type": "text", "text": "Cut off sentence"}]),
        ]
    );

    // A failed stream keeps only what had happened by its last call that ran, whether it was retried, retries ran
    // out, or Esc landed during the retry wait. With no call that ran, nothing is kept.
    let failed_turns = vec![
        assistant(
            "msg_retries_exhausted",
            "same",
            json!([{"type": "text", "text": "Partial visible answer", "native": {"phase": "commentary"}}]),
            Some("error"),
            Some(stream_dropped()),
        ),
        serde_json::from_value(json!({"type": "idle", "id": "msg_idle", "outcome": "failed", "time": {"created": 1}})).unwrap(),
        assistant(
            "msg_tool_ran_before_failure",
            "same",
            json!([
                {"type": "text", "text": "Reading first", "native": {"phase": "commentary"}},
                read_ran("ran"),
                {"type": "text", "text": "Now I'll run the te"},
            ]),
            Some("error"),
            Some(stream_dropped()),
        ),
    ];
    let out = lower(&failed_turns, "same");
    assert_eq!(
        shape(&out),
        vec![("assistant".into(), vec!["text".into(), "tool-call".into()]), ("tool".into(), vec!["tool-result".into()])]
    );
    assert_eq!(out[0]["content"][0], json!({"type": "text", "text": "Reading first", "providerMetadata": {"provider": {"phase": "commentary"}}}));

    let superseded = vec![
        serde_json::from_value(json!({
            "type": "assistant", "id": "msg_superseded", "agent": "build", "model": {"id": "same", "providerID": "provider"},
            "content": [{"type": "text", "text": "Partial broken stream", "native": {"phase": "commentary"}}],
            "retry": {"attempt": 1, "at": 1, "error": stream_dropped()},
            "error": stream_dropped(),
            "time": {"created": 1, "completed": 1},
        }))
        .unwrap(),
        serde_json::from_value(json!({"type": "model-switched", "id": "msg_model", "model": {"id": "same", "providerID": "provider"}, "time": {"created": 1}})).unwrap(),
        assistant(
            "msg_replacement",
            "same",
            json!([{"type": "text", "text": "Clean retry answer", "native": {"phase": "final_answer"}}]),
            Some("stop"),
            None,
        ),
    ];
    let out = lower(&superseded, "same");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["content"], json!([{"type": "text", "text": "Clean retry answer", "providerMetadata": {"provider": {"phase": "final_answer"}}}]));

    // A crashed step is stamped completed when the next step starts but never gets a finish. Cut turns drop calls
    // that never ran and only replay reasoning the model moved past, so a reasoning-only cut is omitted.
    let signed = |text: &str, native: Value, completed: bool| {
        let time = if completed { json!({"created": 1, "completed": 1}) } else { json!({"created": 1}) };
        json!({"type": "reasoning", "text": text, "native": native, "time": time})
    };
    let uncalled = |id: &str, state: Value, executed: Option<bool>| {
        let mut t = json!({"type": "tool", "id": id, "name": "echo", "state": state, "time": {"created": 1}});
        if let Some(e) = executed {
            t["executed"] = json!(e);
        }
        t
    };
    let edge = vec![
        assistant(
            "msg_crash",
            "same",
            json!([
                signed("Signed thought before crash", json!({"signature": "sig_crash"}), true),
                uncalled("stale-streaming", json!({"status": "streaming", "input": "{\"te"}), None),
            ]),
            None,
            None,
        ),
        assistant(
            "msg_whitespace",
            "same",
            json!([
                signed("Signed thought", json!({"signature": "sig_ws"}), true),
                {"type": "text", "text": "\n\n"},
                uncalled("aborted-uncalled", json!({"status": "error", "input": {}, "error": stop_aborted()}), None),
            ]),
            Some("error"),
            Some(stop_aborted()),
        ),
        assistant(
            "msg_multipart_cut",
            "same",
            json!([
                signed("Part 0", json!({"itemId": "rs_cut", "reasoningEncryptedContent": null}), true),
                signed("Part 1", json!({"itemId": "rs_cut", "reasoningEncryptedContent": null}), false),
            ]),
            Some("error"),
            Some(stop_aborted()),
        ),
        assistant(
            "msg_multipart_then_text",
            "same",
            json!([
                signed("Part 0", json!({"itemId": "rs_done", "reasoningEncryptedContent": null}), true),
                signed("Part 1", json!({"itemId": "rs_done", "reasoningEncryptedContent": "encrypted"}), true),
                {"type": "text", "text": "Visible answer before Esc", "native": {"itemId": "msg"}},
            ]),
            Some("error"),
            Some(stop_aborted()),
        ),
    ];
    let out = lower(&edge, "same");
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0]["content"],
        json!([
            {"type": "reasoning", "text": "Part 0", "providerMetadata": {"provider": {"itemId": "rs_done", "reasoningEncryptedContent": null}}},
            {"type": "reasoning", "text": "Part 1", "providerMetadata": {"provider": {"itemId": "rs_done", "reasoningEncryptedContent": "encrypted"}}},
            {"type": "text", "text": "Visible answer before Esc"},
        ])
    );

    // A cut stream drops a hosted call that never ran (marked executed at input start) but keeps input feedback so
    // the model can reissue a malformed call.
    let with_calls = vec![assistant(
        "msg_uncalled_calls",
        "same",
        json!([
            {"type": "text", "text": "Searching"},
            uncalled("hosted-uncalled", json!({"status": "error", "input": {}, "error": stop_aborted()}), Some(true)),
            uncalled("malformed", json!({"status": "error", "input": {"raw": "{"}, "error": {"type": "tool.input-json", "message": "Reissue the call with valid JSON arguments."}}), None),
        ]),
        Some("error"),
        Some(stop_aborted()),
    )];
    let out = lower(&with_calls, "same");
    assert_eq!(
        shape(&out),
        vec![("assistant".into(), vec!["text".into(), "tool-call".into()]), ("tool".into(), vec!["tool-result".into()])]
    );
    assert_eq!(out[0]["content"][1]["type"], "tool-call");
    assert_eq!(out[0]["content"][1]["id"], "malformed");
}

#[test]
fn replays_tools_migrated_from_v1_which_never_recorded_when_they_ran() {
    let migrated = |id: &str, error: Option<Value>| {
        let finish = if error.is_none() { "tool-calls" } else { "error" };
        assistant(
            &format!("msg_{id}"),
            "same",
            json!([{"type": "tool", "id": format!("{id}-read"), "name": "read",
                "state": {"status": "completed", "input": {"path": "README.md"}, "content": [{"type": "text", "text": "Hello"}]},
                "time": {"created": 1, "completed": 1}}]),
            Some(finish),
            error,
        )
    };
    let out = lower(&[migrated("finished", None), migrated("aborted", Some(stop_aborted())), migrated("failed", Some(stream_dropped()))], "same");
    let ids: Vec<Vec<String>> = out
        .iter()
        .map(|m| {
            m["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| if p["type"] == "tool-call" { p["id"].as_str().unwrap().to_string() } else { p["type"].as_str().unwrap().to_string() })
                .collect()
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            vec!["finished-read".to_string()],
            vec!["tool-result".into()],
            vec!["aborted-read".into()],
            vec!["tool-result".into()],
            vec!["failed-read".into()],
            vec!["tool-result".into()],
        ]
    );
}
