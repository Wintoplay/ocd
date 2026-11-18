//! opencode 2.3.1 API behaviour in a release build: the schedule routes are gone, the automation routes check their
//! input, sessions, directories and stored rows and then answer that automations are disabled, the video routes
//! answer that video generation is disabled, and blobs under `<data>/blobs/` are served with an immutable ETag,
//! `If-None-Match`, a single `bytes=` range and a strong `If-Range`. Expected bodies and headers were checked against
//! the 2.3.1 binary.

mod common;

use common::*;
use serde_json::{Value, json};

const AUTOMATIONS_OFF: &str = "Automations are disabled; set experimental.automations in config";
const VIDEO_OFF: &str = "Video generation is disabled; enable experimental.video_generate";
const MISSING_SESSION: &str = "ses_ffffffffffffZZZZZZZZZZZZZZ";
const MISSING_AUTOMATION: &str = "aut_ffffffffffffZZZZZZZZZZZZZZ";

fn res(r: &Res) -> (u16, Value) {
    (r.status.as_u16(), r.json())
}

fn forbidden(message: &str) -> (u16, Value) {
    (403, json!({ "_tag": "ForbiddenError", "message": message }))
}

fn invalid(message: &str, kind: &str) -> (u16, Value) {
    (400, json!({ "_tag": "InvalidRequestError", "message": message, "kind": kind }))
}

fn every(extra: Value) -> Value {
    let mut body = json!({ "name": "n", "trigger": { "type": "every", "every": "1 hour" }, "action": { "type": "prompt", "prompt": "p" } });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

#[tokio::test]
async fn the_schedule_routes_are_gone() {
    let h = harness();
    let id = h.session().await;
    assert_eq!(h.get(&format!("/api/experimental/schedule?sessionID={id}")).await.status, 404);
    assert_eq!(h.post("/api/experimental/schedule", json!({ "sessionID": id, "prompt": "p", "when": { "in": "1h" } })).await.status, 404);
    assert_eq!(h.get("/api/experimental/schedule/sch_ffffffffffffZZZZZZZZZZZZZZ").await.status, 404);
    assert_eq!(h.req("POST", "/api/experimental/schedule/sch_ffffffffffffZZZZZZZZZZZZZZ/pause", None).await.status, 404);
    assert_eq!(h.req("DELETE", "/api/experimental/schedule/sch_ffffffffffffZZZZZZZZZZZZZZ", None).await.status, 404);
}

#[tokio::test]
async fn creating_an_automation_checks_its_input_then_answers_disabled() {
    let h = harness();
    let sid = h.session().await;
    let route = "/api/experimental/automation";
    assert_eq!(res(&h.post(route, every(json!({}))).await), forbidden(AUTOMATIONS_OFF));
    assert_eq!(res(&h.post(route, every(json!({ "sessionID": sid }))).await), forbidden(AUTOMATIONS_OFF));
    assert_eq!(res(&h.post(route, every(json!({ "directory": h.dir() }))).await), forbidden(AUTOMATIONS_OFF));
    assert_eq!(res(&h.post(route, every(json!({ "directory": "rel" }))).await), invalid("Directory must be an absolute path: rel", "automation"));
    let gone = h.cwd.join("gone").display().to_string();
    assert_eq!(
        res(&h.post(route, every(json!({ "directory": gone }))).await),
        invalid(&format!("Directory does not exist: {gone}"), "automation")
    );
    let r = h.post(route, every(json!({ "sessionID": MISSING_SESSION }))).await;
    assert_eq!(
        res(&r),
        (404, json!({ "_tag": "SessionNotFoundError", "sessionID": MISSING_SESSION, "message": format!("Session not found: {MISSING_SESSION}") }))
    );
    // The payload is decoded first.
    assert_eq!(res(&h.post(route, json!({})).await), invalid("Missing key\n  at [\"name\"]", "Payload"));
    assert_eq!(res(&h.post(route, every(json!({ "name": "" }))).await), invalid("Expected a value with a length of at least 1\n  at [\"name\"]", "Payload"));
    assert_eq!(
        res(&h.post(route, json!({ "name": "n", "trigger": { "every": "1h" }, "action": { "prompt": "p" } })).await),
        invalid("Expected Automation.Trigger\n  at [\"trigger\"]", "Payload")
    );
    assert_eq!(
        res(&h.post(route, every(json!({ "action": { "type": "prompt", "prompt": "p", "to": { "session": "ses_x", "subagent": "a" } } }))).await),
        invalid("Expected never\n  at [\"action\"][\"to\"][\"subagent\"]\nExpected never\n  at [\"action\"][\"to\"][\"session\"]", "Payload")
    );
    assert_eq!(
        res(&h.post(route, every(json!({ "action": { "type": "code", "code": "" } }))).await),
        invalid("Expected a value with a length of at least 1\n  at [\"action\"][\"code\"]", "Payload")
    );
}

#[tokio::test]
async fn listing_automations_checks_the_filters_then_answers_disabled() {
    let h = harness();
    let sid = h.session().await;
    let route = "/api/experimental/automation";
    assert_eq!(res(&h.get(route).await), (200, json!({ "data": [] })));
    assert_eq!(res(&h.get(&format!("{route}?sessionID={sid}")).await), forbidden(AUTOMATIONS_OFF));
    assert_eq!(res(&h.get(&format!("{route}?directory={}", enc(&h.dir()))).await), forbidden(AUTOMATIONS_OFF));
    // A folder that is gone stays readable.
    let gone = h.cwd.join("gone").display().to_string();
    assert_eq!(res(&h.get(&format!("{route}?directory={}", enc(&gone))).await), (200, json!({ "data": [] })));
    assert_eq!(res(&h.get(&format!("{route}?directory=rel")).await), invalid("Directory must be an absolute path: rel", "automation"));
    assert_eq!(res(&h.get(&format!("{route}?sessionID={MISSING_SESSION}")).await).0, 404);
    assert_eq!(res(&h.get(&format!("{route}?sessionID=bad")).await), invalid("Expected a string starting with \"ses\"\n  at [\"sessionID\"]", "Query"));
}

#[tokio::test]
async fn a_missing_automation_is_not_found_and_a_stored_one_is_forbidden() {
    let h = harness();
    let missing = json!({
        "_tag": "AutomationNotFoundError",
        "automationID": MISSING_AUTOMATION,
        "message": format!("Automation not found: {MISSING_AUTOMATION}"),
    });
    let base = format!("/api/experimental/automation/{MISSING_AUTOMATION}");
    assert_eq!(res(&h.get(&base).await), (404, missing.clone()));
    assert_eq!(res(&h.req("DELETE", &base, None).await), (404, missing.clone()));
    assert_eq!(res(&h.req("POST", &format!("{base}/pause"), None).await), (404, missing.clone()));
    assert_eq!(res(&h.req("POST", &format!("{base}/resume"), None).await), (404, missing));
    assert_eq!(
        res(&h.get("/api/experimental/automation/bad").await),
        invalid("Expected a string starting with \"aut_\"\n  at [\"automationID\"]", "Params")
    );
    // An automation another release created, or one copied from a schedule, reads as forbidden.
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO automation (id, project_id, directory, name, \"trigger\", tz, action, time_created, time_updated) VALUES ('aut_1', 'global', ?, 'n', '{}', 'UTC', '{}', 0, 0)",
                [h.dir()],
            )
        })
        .unwrap();
    assert_eq!(res(&h.get("/api/experimental/automation/aut_1").await), forbidden(AUTOMATIONS_OFF));
    assert_eq!(res(&h.req("POST", "/api/experimental/automation/aut_1/pause", None).await), forbidden(AUTOMATIONS_OFF));
    assert_eq!(res(&h.req("DELETE", "/api/experimental/automation/aut_1", None).await), forbidden(AUTOMATIONS_OFF));
    assert_eq!(res(&h.get("/api/experimental/automation").await), (200, json!({ "data": [] })));
}

#[tokio::test]
async fn video_generation_answers_disabled_after_decoding() {
    let h = harness();
    let route = "/api/experimental/generate/video";
    assert_eq!(res(&h.post(route, json!({ "model": { "providerID": "x", "id": "y" }, "prompt": "p" })).await), forbidden(VIDEO_OFF));
    assert_eq!(res(&h.post(route, json!({})).await), invalid("Missing key\n  at [\"model\"]", "Payload"));
    assert_eq!(res(&h.post(route, json!({ "model": "x/y", "prompt": "p" })).await), invalid("Expected Model.Ref\n  at [\"model\"]", "Payload"));
    assert_eq!(res(&h.get(&format!("{route}/video_0123456789abABCDEFGHIJKLMN")).await), forbidden(VIDEO_OFF));
    assert_eq!(
        res(&h.get(&format!("{route}/bad")).await),
        invalid("Expected a string matching the RegExp ^video_[0-9a-f]{12}[0-9A-Za-z]{14}$\n  at [\"id\"]", "Params")
    );
}

async fn blob(h: &Harness, id: &str, headers: &[(&str, &str)]) -> Res {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let mut b = Request::builder().method("GET").uri(format!("/api/experimental/blob/{id}"));
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res = h.router.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Res { status, body, headers }
}

fn header<'a>(r: &'a Res, name: &str) -> Option<&'a str> {
    r.headers.get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn blobs_are_served_from_the_data_folder_with_ranges_and_validators() {
    let h = harness();
    let dir = h.app.global.data.join("blobs");
    std::fs::create_dir_all(&dir).unwrap();
    let text = "video_0123456789abABCDEFGHIJKLMN_1";
    std::fs::write(dir.join(text), "hello blob").unwrap();
    let mp4 = "video_0123456789abABCDEFGHIJKLMN_0";
    std::fs::write(dir.join(mp4), [b"\0\0\0\x18ftypisom".as_slice(), &[7u8; 64]].concat()).unwrap();

    let r = blob(&h, text, &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"hello blob");
    for (name, value) in [
        ("content-type", "application/octet-stream"),
        ("content-disposition", "attachment"),
        ("content-security-policy", "default-src 'none'; img-src 'self'; media-src 'self'; sandbox"),
        ("x-content-type-options", "nosniff"),
        ("cache-control", "private, max-age=604800, immutable"),
        ("etag", "\"video_0123456789abABCDEFGHIJKLMN_1\""),
        ("accept-ranges", "bytes"),
        ("content-length", "10"),
    ] {
        assert_eq!(header(&r, name), Some(value), "{name}");
    }
    // The type is read from the first bytes.
    assert_eq!(header(&blob(&h, mp4, &[]).await, "content-type"), Some("video/mp4"));

    let r = blob(&h, text, &[("range", "bytes=2-5")]).await;
    assert_eq!((r.status.as_u16(), &r.body[..], header(&r, "content-range")), (206, b"llo ".as_slice(), Some("bytes 2-5/10")));
    let r = blob(&h, text, &[("range", "bytes=-3")]).await;
    assert_eq!((r.status.as_u16(), &r.body[..], header(&r, "content-range")), (206, b"lob".as_slice(), Some("bytes 7-9/10")));
    let r = blob(&h, text, &[("range", "bytes=50-")]).await;
    assert_eq!((r.status.as_u16(), header(&r, "content-range"), header(&r, "etag")), (416, Some("bytes */10"), Some("\"video_0123456789abABCDEFGHIJKLMN_1\"")));
    // Only a strong If-Range match allows the range.
    let r = blob(&h, text, &[("range", "bytes=2-5"), ("if-range", "\"other\"")]).await;
    assert_eq!((r.status.as_u16(), &r.body[..]), (200, b"hello blob".as_slice()));
    let r = blob(&h, text, &[("range", "bytes=2-5"), ("if-range", "\"video_0123456789abABCDEFGHIJKLMN_1\"")]).await;
    assert_eq!(r.status, 206);
    let r = blob(&h, text, &[("if-none-match", "W/\"video_0123456789abABCDEFGHIJKLMN_1\"")]).await;
    assert_eq!((r.status.as_u16(), r.body.len(), header(&r, "cache-control")), (304, 0, Some("private, max-age=604800, immutable")));

    let missing = "video_0123456789abABCDEFGHIJKLMN_2";
    assert_eq!(
        res(&blob(&h, missing, &[]).await),
        (404, json!({ "_tag": "BlobNotFoundError", "blobID": missing, "message": format!("Blob not found: {missing}") }))
    );
    assert_eq!(
        res(&blob(&h, "bad", &[]).await),
        invalid("Expected a string matching the RegExp ^video_[0-9a-f]{12}[0-9A-Za-z]{14}_(0|[1-9][0-9]{0,2})$\n  at [\"id\"]", "Params")
    );
}

#[tokio::test]
async fn plugin_list_names_the_automation_tool_plugin() {
    let h = harness();
    let r = h.get("/api/plugin").await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let text = r.body.iter().map(|b| *b as char).collect::<String>();
    assert!(text.contains("\"opencode.tool.automation\""), "{text}");
    assert!(!text.contains("opencode.tool.schedule"));
}
