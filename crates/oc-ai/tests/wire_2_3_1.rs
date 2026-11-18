//! Reasoning replay on OpenAI Chat routes (opencode 2.3.1, #64243): a model that declares a reasoning field other
//! than `reasoning_details` no longer replays earlier turns' `reasoning_details`/`reasoning_opaque`, and a model that
//! declares `reasoning_details` keeps the structured array instead of having the reasoning text written over it.
//! Ports of the new packages/ai/test `provider/openai-chat.test.ts` cases.
//!
//! With `OCD_SCRATCH` (holding the 2.3.1 checkout `src/opencode-2.3.1`) and `OCD_BUN_PATH`, replayed request bodies
//! on the OpenAI-compatible routes are also compiled with upstream 2.3.1's own routes and compared.

use oc_ai::protocols::openai_chat as oc;
use oc_ai::route::auth;
use oc_ai::testing::compile_body;
use oc_ai::*;
use serde_json::{json, Value};

fn model() -> LanguageModel {
    oc::route().base_url(Some("https://api.openai.test/v1/".into())).auth(auth::none()).model("gpt-4o-mini").unwrap()
}

fn declaring(field: &str) -> LanguageModel {
    model().with_compatibility(Compatibility { reasoning_field: Some(field.into()), ..Default::default() })
}

fn details() -> Value {
    json!([{"type": "reasoning.text", "text": "thinking", "signature": "signed", "format": "anthropic-claude-v1", "index": 0}])
}

/// The replayed messages of one assistant turn with structured reasoning observed under the `reasoning` field.
async fn replay(model: LanguageModel) -> Value {
    let message: Message = serde_json::from_value(json!({"role": "assistant", "content": [
        {"type": "reasoning", "text": "thinking", "providerMetadata": {"openai": {"reasoningField": "reasoning", "reasoningDetails": details()}}},
        {"type": "text", "text": "Hello"},
    ]}))
    .unwrap();
    let mut req = LLMRequest::new(model);
    req.messages = vec![message];
    compile_body(&req).await.unwrap()["messages"].clone()
}

/// "does not replay reasoning details when the model declares another reasoning field".
#[tokio::test]
async fn declared_field_drops_replayed_details() {
    assert_eq!(replay(declaring("reasoning_content")).await, json!([{"role": "assistant", "content": "Hello", "reasoning_content": "thinking"}]));
}

/// "keeps replayed reasoning details when the model declares reasoning_details".
#[tokio::test]
async fn declared_reasoning_details_keeps_the_array() {
    assert_eq!(replay(declaring("reasoning_details")).await, json!([{"role": "assistant", "content": "Hello", "reasoning_details": details()}]));
}

/// "keeps replayed reasoning details when the model declares no reasoning field".
#[tokio::test]
async fn undeclared_field_keeps_details_and_text() {
    assert_eq!(
        replay(model()).await,
        json!([{"role": "assistant", "content": "Hello", "reasoning": "thinking", "reasoning_details": details()}])
    );
}

/// A Copilot-style opaque value is dropped too when another field is declared, and kept when none is.
#[tokio::test]
async fn declared_field_drops_replayed_opaque() {
    let message: Message = serde_json::from_value(json!({"role": "assistant", "content": [
        {"type": "reasoning", "text": "Read it.", "providerMetadata": {"openai": {"reasoningField": "reasoning_text", "reasoningOpaque": "opaque"}}},
        {"type": "text", "text": "Done"},
    ]}))
    .unwrap();
    let body = |model: LanguageModel| {
        let mut req = LLMRequest::new(model);
        req.messages = vec![message.clone()];
        async move { compile_body(&req).await.unwrap()["messages"].clone() }
    };
    assert_eq!(body(declaring("reasoning_content")).await, json!([{"role": "assistant", "content": "Done", "reasoning_content": "Read it."}]));
    assert_eq!(
        body(model()).await,
        json!([{"role": "assistant", "content": "Done", "reasoning_opaque": "opaque", "reasoning_text": "Read it."}])
    );
}

// ---------------------------------------------------------------------------------------------
// Cross-check with upstream 2.3.1's own routes (Bun on the checkout)
// ---------------------------------------------------------------------------------------------

/// The 2.3.1 checkout's `packages/ai` and the Bun binary, when both are available.
fn upstream() -> Option<(std::path::PathBuf, String)> {
    let (Ok(scratch), Ok(bun)) = (std::env::var("OCD_SCRATCH"), std::env::var("OCD_BUN_PATH")) else { return None };
    let ai = std::path::PathBuf::from(scratch).join("src/opencode-2.3.1/packages/ai");
    // The 2.3.1 checkout (its replay guard), not an older one.
    let chat = std::fs::read_to_string(ai.join("src/protocols/openai-chat.ts")).ok()?;
    chat.contains("replaysStructured").then_some((ai, bun))
}

/// Runs `script` (TypeScript) under Bun in the checkout with `input` as JSON and returns its JSON output.
fn run_bun(ai: &std::path::Path, bun: &str, script: &str, input: &Value) -> Value {
    // In the checkout, so its packages resolve (Bun never installs them).
    let dir = tempfile::tempdir_in(ai).unwrap();
    let path = dir.path().join("check.ts");
    std::fs::write(&path, script.replace("$AI", &format!("{:?}", ai.display().to_string()))).unwrap();
    let data = dir.path().join("in.json");
    std::fs::write(&data, serde_json::to_string(input).unwrap()).unwrap();
    let output = std::process::Command::new(bun)
        .arg("run")
        .arg("--no-install")
        .arg(&path)
        .arg(&data)
        .arg(dir.path().join("out.json"))
        .current_dir(ai)
        .output()
        .unwrap();
    assert!(output.status.success(), "bun failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&std::fs::read(dir.path().join("out.json")).unwrap()).unwrap()
}

const COMPILE_SCRIPT: &str = r#"import { Effect } from "effect"
import { HttpClientRequest } from "effect/http"
import { readFileSync, writeFileSync } from "node:fs"
const ai = $AI
const { ai: AI, LanguageModel } = await import(ai + "/src/effect.ts")
const { LLMClient } = await import(ai + "/src/route.ts")
const { dynamicResponse } = await import(ai + "/test/lib/http.ts")
const cases = JSON.parse(readFileSync(process.argv[2], "utf8"))
const out = []
for (const item of cases) {
  const pkg = await import(ai + "/src/providers/" + item.package + ".ts")
  let model = pkg.model(item.model, item.settings)
  if (item.compatibility) model = LanguageModel.update(model, { compatibility: item.compatibility })
  let sent
  const exit = await Effect.runPromiseExit(
    Effect.suspend(() => LLMClient.generate(AI.request({ model, messages: item.messages }))).pipe(
      Effect.provide(
        dynamicResponse((input) =>
          Effect.gen(function* () {
            const web = yield* HttpClientRequest.toWeb(input.request).pipe(Effect.orDie)
            sent = { url: web.url, body: input.text }
            return input.respond("{}", { status: 400 })
          }),
        ),
      ),
    ),
  )
  if (sent) out.push(sent)
  else out.push({ error: exit.cause?.reasons?.[0]?.error?.reason?.message ?? String(exit.cause) })
}
writeFileSync(process.argv[3], JSON.stringify(out))
"#;

/// Assistant turns as each route's earlier responses leave them, under every metadata key the routes use.
fn histories() -> Vec<Value> {
    let keys = ["openai", "openrouter", "openai-compatible", "deepseek", "moonshot", "vercel-ai-gateway", "fireworks", "togetherai", "custom", "github-copilot"];
    let meta = |m: Value| Value::Object(keys.iter().map(|k| (k.to_string(), m.clone())).collect());
    let reasoning = |text: &str, m: Value| json!({"type": "reasoning", "text": text, "providerMetadata": meta(m)});
    let user = json!({"role": "user", "content": [{"type": "text", "text": "Go on."}]});
    let turn = |content: Value| json!([{"role": "user", "content": [{"type": "text", "text": "Start."}]}, {"role": "assistant", "content": content}, user.clone()]);
    let signed = json!([{"type": "reasoning.text", "text": "thinking", "signature": "signed", "format": "anthropic-claude-v1", "index": 0}]);
    let encrypted = json!([{"type": "reasoning.encrypted", "data": "enc", "format": "openai-responses-v1", "index": 0}]);
    let call = json!({"type": "tool-call", "id": "call_1", "name": "read", "input": {"path": "a"}});
    let result = json!({"role": "tool", "content": [{"type": "tool-result", "id": "call_1", "name": "read", "result": {"type": "text", "value": "ok"}}]});
    vec![
        // OpenRouter / AI Gateway: structured details observed under `reasoning`.
        turn(json!([reasoning("thinking", json!({"reasoningField": "reasoning", "reasoningDetails": signed})), {"type": "text", "text": "Hello"}])),
        // Structured details with no observed field, and an encrypted detail.
        turn(json!([reasoning("", json!({"reasoningDetails": encrypted})), {"type": "text", "text": "Hello"}])),
        // DeepSeek-style scalar reasoning.
        turn(json!([reasoning("plain", json!({"reasoningField": "reasoning_content"})), {"type": "text", "text": "Hi"}])),
        // Copilot: interleaved reasoning with opaque values and a tool call.
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Read."}]},
            {"role": "assistant", "content": [
                reasoning("Read it.", json!({"reasoningField": "reasoning_text", "reasoningOpaque": "opaque-1"})),
                reasoning(" Then more.", json!({"reasoningField": "reasoning_text", "reasoningOpaque": "opaque-2"})),
                call.clone(),
            ]},
            result.clone(),
        ]),
        // Opaque only, no reasoning text.
        turn(json!([reasoning("", json!({"reasoningOpaque": "opaque"})), {"type": "text", "text": "Hello"}])),
        // Mixed: one structured part, one scalar part, then a tool loop.
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Read."}]},
            {"role": "assistant", "content": [
                reasoning("first", json!({"reasoningField": "reasoning", "reasoningDetails": signed})),
                reasoning(" second", json!({})),
                {"type": "text", "text": "Reading."},
                call.clone(),
            ]},
            result.clone(),
        ]),
        // Reasoning without metadata, and no reasoning at all.
        turn(json!([{"type": "reasoning", "text": "bare"}, {"type": "text", "text": "Hello"}])),
        turn(json!([{"type": "text", "text": "Hello"}])),
    ]
}

fn replay_cases() -> Vec<Value> {
    let routes = [
        ("openrouter", "anthropic/claude-sonnet-4.6", json!({"apiKey": "test"})),
        ("openai-compatible", "deepseek-reasoner", json!({"apiKey": "test", "baseURL": "https://chat.test/v1"})),
        ("openai/chat", "gpt-5.4", json!({"apiKey": "test"})),
        ("vercel-ai-gateway/chat", "anthropic/claude-sonnet-4.6", json!({"apiKey": "test"})),
        ("deepseek", "deepseek-reasoner", json!({"apiKey": "test"})),
        ("moonshot", "kimi-k2-thinking", json!({"apiKey": "test"})),
        ("fireworks", "accounts/fireworks/models/kimi-k2-thinking", json!({"apiKey": "test"})),
        ("togetherai", "moonshotai/Kimi-K2-Thinking", json!({"apiKey": "test"})),
    ];
    let compat = [
        Value::Null,
        json!({"reasoningField": "reasoning_content"}),
        json!({"reasoningField": "reasoning_details"}),
        json!({"reasoningField": "reasoning"}),
        json!({"reasoningField": "reasoning_text"}),
        json!({"reasoningField": "reasoning_details", "requireReasoning": true}),
        json!({"reasoningField": "reasoning_content", "requireReasoning": true}),
        json!({"requireReasoning": true}),
    ];
    let mut cases = vec![];
    for (package, id, settings) in &routes {
        for compatibility in &compat {
            for messages in histories() {
                cases.push(json!({"package": package, "model": id, "settings": settings, "compatibility": compatibility, "messages": messages}));
            }
        }
    }
    cases
}

/// Compiles replayed reasoning on the OpenAI Chat routes, including OpenRouter, with each declared reasoning field,
/// with upstream 2.3.1's routes and with ocd, and compares the URL and the body text (key order included). Needs
/// `OCD_SCRATCH` and `OCD_BUN_PATH`; skipped otherwise.
#[tokio::test]
async fn replayed_reasoning_matches_upstream_2_3_1() {
    let Some((ai, bun)) = upstream() else { return };
    let cases = replay_cases();
    let mut ours = vec![];
    for case in &cases {
        let mut model =
            oc_ai::providers::language_model(&format!("@opencode/ai/providers/{}", case["package"].as_str().unwrap()), case["model"].as_str().unwrap(), case["settings"].as_object().unwrap())
                .unwrap();
        // `LanguageModel.update` replaces the compatibility as a whole.
        if !case["compatibility"].is_null() {
            model.compatibility = Some(serde_json::from_value(case["compatibility"].clone()).unwrap());
        }
        let mut req = LLMRequest::new(model);
        req.messages = serde_json::from_value(case["messages"].clone()).unwrap();
        ours.push(match Client::new().compile(&req).await {
            Ok(compiled) => json!({"url": compiled.prepared.url, "body": compiled.prepared.body}),
            Err(error) => json!({"error": error.message}),
        });
    }
    let theirs = run_bun(&ai, &bun, COMPILE_SCRIPT, &Value::Array(cases.clone()));
    let theirs = theirs.as_array().unwrap();
    let differing: Vec<String> = theirs
        .iter()
        .zip(ours.iter())
        .enumerate()
        .filter(|(_, (theirs, ours))| *theirs != *ours)
        .map(|(i, (theirs, ours))| format!("case {i} ({} {})\n  upstream: {theirs}\n  ocd:      {ours}", cases[i]["package"], cases[i]["compatibility"]))
        .collect();
    eprintln!("compared {} requests with upstream 2.3.1", theirs.len());
    assert!(differing.is_empty(), "{} of {} differ:\n{}", differing.len(), cases.len(), differing.join("\n"));
    // The replay paths the fix is about were reached.
    let bodies: Vec<&str> = theirs.iter().filter_map(|t| t["body"].as_str()).collect();
    assert!(bodies.iter().any(|b| b.contains("\"reasoning_details\":[")));
    assert!(bodies.iter().all(|b| !b.contains("\"reasoning_details\":\"")));
}
