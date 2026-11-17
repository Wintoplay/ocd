//! Freeform tool input (opencode 2.3.0, #61703): tools that declare `freeform` go out as OpenAI custom tools on the
//! Responses and Chat routes, custom tool calls stream back as JSON-argument deltas of the tool's one string field,
//! and past calls replay as the type their tool is declared with now. Ports of packages/ai/test
//! `provider/freeform-tools.test.ts` (the `openai-freeform.test.ts` unit cases live in
//! `src/protocols/openai_freeform.rs`).
//!
//! With `OCD_SCRATCH` (holding the 2.3.0 checkout `src/opencode-2.3.0`) and `OCD_BUN_PATH`, request bodies and
//! decoded streams are also compared with upstream 2.3.0's own routes.

use oc_ai::testing::{compile_body, decode};
use oc_ai::*;
use serde_json::{json, Map, Value};

const GRAMMAR: &str = r#"start: "ZZ" /[0-9]{3}/"#;

fn input_schema() -> Value {
    json!({"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"], "additionalProperties": false})
}

fn run_tool(freeform: Option<Freeform>) -> ToolDefinition {
    ToolDefinition::new("run", "Run code", input_schema()).with_freeform(freeform)
}

fn lark() -> Option<Freeform> {
    Some(Freeform::Lark { grammar: GRAMMAR.into() })
}

fn package(name: &str, id: &str, settings: Value) -> LanguageModel {
    oc_ai::providers::language_model(&format!("@opencode/ai/providers/{name}"), id, settings.as_object().unwrap()).unwrap()
}

fn responses() -> LanguageModel {
    package("openai", "gpt-5.4", json!({"apiKey": "test"}))
}

fn chat() -> LanguageModel {
    package("openai/chat", "gpt-5.4", json!({"apiKey": "test"}))
}

/// `body(model, freeform)`: the compiled body of "Run it." with the `run` tool chosen.
async fn body(model: LanguageModel, freeform: Option<Freeform>) -> Value {
    let mut req = LLMRequest::new(model).prompt("Run it.").tool(run_tool(freeform));
    req.tool_choice = Some(ToolChoice::from_str("run"));
    compile_body(&req).await.unwrap()
}

fn sse(values: &[Value]) -> Vec<u8> {
    values.iter().map(|v| format!("data: {v}\n\n")).collect::<String>().into_bytes()
}

fn generate(req: &LLMRequest, frames: &[Value]) -> LLMResponse {
    let events: Vec<LLMEvent> = decode(req, &sse(frames)).into_iter().map(|e| e.unwrap()).collect();
    LLMResponse::from_events(events).unwrap()
}

fn deltas(response: &LLMResponse) -> Vec<String> {
    response
        .events
        .iter()
        .filter_map(|e| match e {
            LLMEvent::ToolInputDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn tool_calls(response: &LLMResponse) -> Vec<Value> {
    response.tool_calls().into_iter().map(|e| serde_json::to_value(e).unwrap()).collect()
}

fn responses_lark() -> Value {
    json!({"type": "custom", "name": "run", "description": "Run code", "format": {"type": "grammar", "syntax": "lark", "definition": GRAMMAR}})
}

fn chat_lark() -> Value {
    json!({"type": "custom", "custom": {"name": "run", "description": "Run code", "format": {"type": "grammar", "grammar": {"syntax": "lark", "definition": GRAMMAR}}}})
}

/// freeform-tools.test.ts "encodes each format as a Responses custom tool".
#[tokio::test]
async fn encodes_each_format_as_a_responses_custom_tool() {
    assert_eq!(
        body(responses(), Some(Freeform::Text)).await["tools"],
        json!([{"type": "custom", "name": "run", "description": "Run code", "format": {"type": "text"}}])
    );
    assert_eq!(
        body(responses(), Some(Freeform::Regex { pattern: "ZZ[0-9]{3}".into() })).await["tools"],
        json!([{"type": "custom", "name": "run", "description": "Run code", "format": {"type": "grammar", "syntax": "regex", "definition": "ZZ[0-9]{3}"}}])
    );
    let prepared = body(responses(), lark()).await;
    assert_eq!(prepared["tools"], json!([responses_lark()]));
    assert_eq!(prepared["tool_choice"], json!({"type": "custom", "name": "run"}));
}

/// freeform-tools.test.ts "encodes each format as a Chat custom tool".
#[tokio::test]
async fn encodes_each_format_as_a_chat_custom_tool() {
    assert_eq!(
        body(chat(), Some(Freeform::Text)).await["tools"],
        json!([{"type": "custom", "custom": {"name": "run", "description": "Run code", "format": {"type": "text"}}}])
    );
    assert_eq!(
        body(chat(), Some(Freeform::Regex { pattern: "ZZ[0-9]{3}".into() })).await["tools"],
        json!([{"type": "custom", "custom": {"name": "run", "description": "Run code", "format": {"type": "grammar", "grammar": {"syntax": "regex", "definition": "ZZ[0-9]{3}"}}}}])
    );
    let prepared = body(chat(), lark()).await;
    assert_eq!(prepared["tools"], json!([chat_lark()]));
    assert_eq!(prepared["tool_choice"], json!({"type": "custom", "custom": {"name": "run"}}));
}

/// freeform-tools.test.ts "encodes custom tools in every route that reuses the OpenAI wire format".
#[tokio::test]
async fn encodes_custom_tools_in_every_route_that_reuses_the_openai_wire_format() {
    let responses_models = [
        package("azure/responses", "gpt-5.4", json!({"apiKey": "test", "resourceName": "fixture"})),
        package("amazon-bedrock/mantle/responses", "openai.gpt-5.4", json!({"apiKey": "test", "region": "us-east-1"})),
        package("vercel-ai-gateway/responses", "openai/gpt-5.4", json!({"apiKey": "test"})),
        package("openai-compatible-responses", "gpt-5.4", json!({"apiKey": "test", "baseURL": "https://responses.test/v1"})),
        package("openrouter/responses", "openai/gpt-5.4", json!({"apiKey": "test"})),
        package("digitalocean/responses", "openai-gpt-5.4", json!({"apiKey": "test"})),
        package("venice/responses", "openai-gpt-54", json!({"apiKey": "test"})),
        package("xai", "grok-4", json!({"apiKey": "test"})),
    ];
    let chat_models = [
        package("azure/chat", "gpt-5.4", json!({"apiKey": "test", "resourceName": "fixture"})),
        package("amazon-bedrock/mantle/chat", "openai.gpt-5.4", json!({"apiKey": "test", "region": "us-east-1"})),
        package("vercel-ai-gateway/chat", "openai/gpt-5.4", json!({"apiKey": "test"})),
        package("openrouter", "openai/gpt-5.4", json!({"apiKey": "test"})),
        package("openai-compatible", "gpt-5.4", json!({"apiKey": "test", "baseURL": "https://chat.test/v1"})),
    ];
    for model in responses_models {
        let id = model.route.id.clone();
        assert_eq!(body(model, lark()).await["tools"], json!([responses_lark()]), "{id}");
    }
    for model in chat_models {
        let id = model.route.id.clone();
        assert_eq!(body(model, lark()).await["tools"], json!([chat_lark()]), "{id}");
    }
}

/// freeform-tools.test.ts "sends JSON tools for tools without freeform and for other protocols".
#[tokio::test]
async fn sends_json_tools_for_tools_without_freeform_and_for_other_protocols() {
    let prepared = body(responses(), None).await;
    assert_eq!((&prepared["tools"][0]["type"], &prepared["tools"][0]["name"]), (&json!("function"), &json!("run")));
    assert_eq!(prepared["tool_choice"], json!({"type": "function", "name": "run"}));
    let prepared = body(chat(), None).await;
    assert_eq!((&prepared["tools"][0]["type"], &prepared["tools"][0]["function"]["name"]), (&json!("function"), &json!("run")));
    let prepared = body(package("anthropic", "claude-sonnet-4-5", json!({"apiKey": "test"})), lark()).await;
    assert_eq!(prepared["tools"][0]["name"], "run");
    assert_eq!(prepared["tools"][0]["input_schema"]["properties"], json!({"code": {"type": "string"}}));
}

/// freeform-tools.test.ts "streams Responses custom calls as JSON argument deltas".
#[tokio::test]
async fn streams_responses_custom_calls_as_json_argument_deltas() {
    let req = LLMRequest::new(responses()).prompt("Run it.").tool(run_tool(lark()));
    let item = json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "run", "input": ""});
    let mut done_item = item.clone();
    done_item["input"] = json!("ZZ\"1\n");
    let response = generate(&req, &[
        json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
        json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "item_id": "ctc_1", "delta": "ZZ"}),
        json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "item_id": "ctc_1", "delta": "\"1\n"}),
        json!({"type": "response.custom_tool_call_input.done", "output_index": 0, "item_id": "ctc_1", "input": "ZZ\"1\n"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": done_item}),
        json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let d = deltas(&response);
    assert_eq!(d, vec![r#"{"code":""#, "ZZ", r#"\"1\n"#, r#""}"#]);
    assert_eq!(serde_json::from_str::<Value>(&d.concat()).unwrap(), json!({"code": "ZZ\"1\n"}));
    let calls = tool_calls(&response);
    assert_eq!(calls.len(), 1);
    assert_eq!((&calls[0]["id"], &calls[0]["name"], &calls[0]["input"]), (&json!("call_1"), &json!("run"), &json!({"code": "ZZ\"1\n"})));
    assert_eq!(calls[0]["providerMetadata"], json!({"openai": {"itemId": "ctc_1", "custom": true}}));
    assert_eq!(response.finish_reason.normalized, FinishReason::ToolCalls);

    let mut replay = LLMRequest::new(responses());
    replay.messages = vec![response.message];
    replay.tools = vec![ToolEntry::Tool(run_tool(lark()))];
    let replayed = compile_body(&replay).await.unwrap();
    assert_eq!(replayed["input"][0], json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "run", "input": "ZZ\"1\n"}));
}

/// freeform-tools.test.ts "parses a Responses custom call that arrives only when done".
#[test]
fn parses_a_responses_custom_call_that_arrives_only_when_done() {
    let req = LLMRequest::new(responses()).prompt("Run it.").tool(run_tool(lark()));
    let item = json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "run", "input": "ZZ123"});
    let response = generate(&req, &[
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let calls = tool_calls(&response);
    assert_eq!(calls.len(), 1);
    assert_eq!((&calls[0]["id"], &calls[0]["name"], &calls[0]["input"]), (&json!("call_1"), &json!("run"), &json!({"code": "ZZ123"})));
}

/// freeform-tools.test.ts "streams Chat custom calls as JSON argument deltas".
#[test]
fn streams_chat_custom_calls_as_json_argument_deltas() {
    let req = LLMRequest::new(chat()).prompt("Run it.").tool(run_tool(lark()));
    let chunk = |call: Value| {
        let mut tool = json!({"index": 0});
        for (k, v) in call.as_object().unwrap() {
            tool[k] = v.clone();
        }
        json!({"id": "chatcmpl_1", "choices": [{"index": 0, "delta": {"tool_calls": [tool]}, "finish_reason": null}]})
    };
    let response = generate(&req, &[
        chunk(json!({"id": "call_1", "type": "custom", "custom": {"name": "run", "input": ""}})),
        chunk(json!({"custom": {"input": "ZZ"}})),
        chunk(json!({"custom": {"input": "\"1\n"}})),
        json!({"id": "chatcmpl_1", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ]);
    assert_eq!(deltas(&response), vec![r#"{"code":""#, "ZZ", r#"\"1\n"#, r#""}"#]);
    let calls = tool_calls(&response);
    assert_eq!(calls.len(), 1);
    assert_eq!((&calls[0]["id"], &calls[0]["name"], &calls[0]["input"]), (&json!("call_1"), &json!("run"), &json!({"code": "ZZ\"1\n"})));
    assert_eq!(response.finish_reason.normalized, FinishReason::ToolCalls);
}

fn history(metadata: Value) -> Vec<Message> {
    serde_json::from_value(json!([
        {"role": "user", "content": [{"type": "text", "text": "Run it."}]},
        {"role": "assistant", "content": [{"type": "tool-call", "id": "call_1", "name": "run", "input": {"code": "ZZ123"}, "providerMetadata": {"openai": metadata}}]},
        {"role": "tool", "content": [{"type": "tool-result", "id": "call_1", "name": "run", "result": {"type": "json", "value": "ok"}}]},
    ]))
    .unwrap()
}

async fn replayed(model: LanguageModel, declared: Option<Freeform>, metadata: Value, field: &str) -> Vec<Value> {
    let mut req = LLMRequest::new(model);
    req.messages = history(metadata);
    req.tools = vec![ToolEntry::Tool(run_tool(declared))];
    compile_body(&req).await.unwrap()[field].as_array().unwrap()[1..].to_vec()
}

/// freeform-tools.test.ts "replays history the way each tool is declared in the current request".
#[tokio::test]
async fn replays_history_the_way_each_tool_is_declared_in_the_current_request() {
    let custom = json!({"itemId": "ctc_1", "custom": true});
    assert_eq!(
        replayed(responses(), lark(), custom.clone(), "input").await,
        vec![
            json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "run", "input": "ZZ123"}),
            json!({"type": "custom_tool_call_output", "call_id": "call_1", "output": "\"ok\""}),
        ]
    );
    assert_eq!(
        replayed(responses(), None, custom.clone(), "input").await,
        vec![
            json!({"type": "function_call", "call_id": "call_1", "name": "run", "arguments": "{\"code\":\"ZZ123\"}"}),
            json!({"type": "function_call_output", "call_id": "call_1", "output": "\"ok\""}),
        ]
    );
    assert_eq!(
        replayed(responses(), lark(), json!({"itemId": "fc_1"}), "input").await,
        vec![
            json!({"type": "custom_tool_call", "call_id": "call_1", "name": "run", "input": "ZZ123"}),
            json!({"type": "custom_tool_call_output", "call_id": "call_1", "output": "\"ok\""}),
        ]
    );
    assert_eq!(
        replayed(responses(), None, json!({"itemId": "fc_1"}), "input").await,
        vec![
            json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "run", "arguments": "{\"code\":\"ZZ123\"}"}),
            json!({"type": "function_call_output", "call_id": "call_1", "output": "\"ok\""}),
        ]
    );

    let messages = replayed(chat(), lark(), custom.clone(), "messages").await;
    assert_eq!(messages[0]["tool_calls"], json!([{"id": "call_1", "type": "custom", "custom": {"name": "run", "input": "ZZ123"}}]));
    assert_eq!((&messages[1]["role"], &messages[1]["tool_call_id"], &messages[1]["content"]), (&json!("tool"), &json!("call_1"), &json!("\"ok\"")));
    let messages = replayed(chat(), None, custom.clone(), "messages").await;
    assert_eq!(messages[0]["tool_calls"][0]["type"], "function");
    assert_eq!(messages[0]["tool_calls"][0]["function"], json!({"name": "run", "arguments": "{\"code\":\"ZZ123\"}"}));
    let messages = replayed(package("anthropic", "claude-sonnet-4-5", json!({"apiKey": "test"})), lark(), custom, "messages").await;
    assert_eq!(messages[0]["content"][0]["type"], "tool_use");
    assert_eq!(messages[0]["content"][0]["input"], json!({"code": "ZZ123"}));
    assert_eq!(messages[1]["content"][0]["tool_use_id"], "call_1");
}

/// freeform-tools.test.ts "rejects freeform tools without exactly one required string parameter": the protocols
/// send such a tool as a function tool (`ToolDefinition.freeformField` finds no field).
#[test]
fn a_freeform_tool_needs_exactly_one_required_string_parameter() {
    let field = |schema: Value| ToolDefinition::freeform_field(schema.as_object().unwrap());
    assert_eq!(field(json!({"type": "object", "properties": {"code": {"type": "number"}}, "required": ["code"]})), None);
    assert_eq!(field(json!({"type": "object", "properties": {"code": {"type": "string"}}})), None);
    assert_eq!(field(json!({"type": "object", "properties": {"code": {"type": "string"}, "lang": {"type": "string"}}, "required": ["code", "lang"]})), None);
    assert_eq!(field(input_schema()), Some("code".into()));
}

// ---------------------------------------------------------------------------------------------
// Cross-checks with upstream 2.3.0's own routes (Bun on the checkout)
// ---------------------------------------------------------------------------------------------

/// The 2.3.0 checkout's `packages/ai` and the Bun binary, when both are available.
fn upstream() -> Option<(std::path::PathBuf, String)> {
    let (Ok(scratch), Ok(bun)) = (std::env::var("OCD_SCRATCH"), std::env::var("OCD_BUN_PATH")) else { return None };
    let ai = std::path::PathBuf::from(scratch).join("src/opencode-2.3.0/packages/ai");
    ai.join("src/protocols/openai-freeform.ts").is_file().then_some((ai, bun))
}

fn numbers(value: Value) -> Value {
    match value {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.into_iter().map(numbers).collect()),
        Value::Object(o) => Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, numbers(v))).collect()),
        other => other,
    }
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
        .env_remove("OPENAI_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success(), "bun failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&std::fs::read(dir.path().join("out.json")).unwrap()).unwrap()
}

/// A case's request in ocd: prompt or messages, tools (with `freeform`), tool choice.
fn our_request(case: &Value) -> LLMRequest {
    let model = package(case["package"].as_str().unwrap(), case["model"].as_str().unwrap(), case["settings"].clone());
    let spec = &case["request"];
    let mut req = LLMRequest::new(model);
    if let Some(prompt) = spec["prompt"].as_str() {
        req = req.prompt(prompt);
    }
    if let Some(messages) = spec.get("messages") {
        req.messages = serde_json::from_value(messages.clone()).unwrap();
    }
    for tool in spec.get("tools").and_then(Value::as_array).into_iter().flatten() {
        req.tools.push(ToolEntry::Tool(serde_json::from_value(tool.clone()).unwrap()));
    }
    if let Some(choice) = spec["toolChoice"].as_str() {
        req.tool_choice = Some(ToolChoice::from_str(choice));
    }
    req.provider_options = spec.get("providerOptions").and_then(Value::as_object).cloned();
    req
}

const COMPILE_SCRIPT: &str = r#"import { Effect } from "effect"
import { HttpClientRequest } from "effect/http"
import { readFileSync, writeFileSync } from "node:fs"
const ai = $AI
const { ai: AI, ToolDefinition } = await import(ai + "/src/effect.ts")
const { LLMClient } = await import(ai + "/src/route.ts")
const { dynamicResponse } = await import(ai + "/test/lib/http.ts")
const cases = JSON.parse(readFileSync(process.argv[2], "utf8"))
const out = []
for (const item of cases) {
  const pkg = await import(ai + "/src/providers/" + item.package + ".ts")
  const model = pkg.model(item.model, item.settings)
  const { tools, ...rest } = item.request
  let sent
  const exit = await Effect.runPromiseExit(
    Effect.suspend(() =>
      LLMClient.generate(AI.request({ model, ...rest, ...(tools ? { tools: tools.map((t) => ToolDefinition.make(t)) } : {}) })),
    ).pipe(
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

fn compile_cases() -> Vec<Value> {
    let tool = |freeform: Value| {
        let mut t = json!({"name": "run", "description": "Run code", "inputSchema": input_schema()});
        if !freeform.is_null() {
            t["freeform"] = freeform;
        }
        t
    };
    let other = json!({"name": "lookup", "description": "Lookup data", "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}, "n": {"type": "number"}}, "required": ["q"]}});
    let text = json!({"type": "text"});
    let regex = json!({"type": "regex", "pattern": "ZZ[0-9]{3}"});
    let lark = json!({"type": "lark", "grammar": GRAMMAR});
    let history = |metadata: Value| {
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Run it."}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Running."},
                {"type": "tool-call", "id": "call_1", "name": "run", "input": {"code": "ZZ\"123\n"}, "providerMetadata": {"openai": metadata}},
                {"type": "tool-call", "id": "call_2", "name": "lookup", "input": {"q": "x"}, "providerMetadata": {"openai": {"itemId": "fc_2"}}},
            ]},
            {"role": "tool", "content": [
                {"type": "tool-result", "id": "call_1", "name": "run", "result": {"type": "json", "value": "ok"}},
                {"type": "tool-result", "id": "call_2", "name": "lookup", "result": {"type": "text", "value": "found"}},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "Again."}]},
        ])
    };
    let mut cases = vec![];
    let routes = [
        ("openai", "gpt-5.4", json!({"apiKey": "test"})),
        ("openai/chat", "gpt-5.4", json!({"apiKey": "test"})),
        ("azure/responses", "gpt-5.4", json!({"apiKey": "test", "resourceName": "fixture"})),
        ("azure/chat", "gpt-5.4", json!({"apiKey": "test", "resourceName": "fixture"})),
        ("openai-compatible-responses", "gpt-5.4", json!({"apiKey": "test", "baseURL": "https://responses.test/v1"})),
        ("openai-compatible", "gpt-5.4", json!({"apiKey": "test", "baseURL": "https://chat.test/v1"})),
        ("openrouter/responses", "openai/gpt-5.4", json!({"apiKey": "test"})),
        ("openrouter", "openai/gpt-5.4", json!({"apiKey": "test"})),
        ("digitalocean/responses", "openai-gpt-5.4", json!({"apiKey": "test"})),
        ("venice/responses", "openai-gpt-54", json!({"apiKey": "test"})),
        ("vercel-ai-gateway/responses", "openai/gpt-5.4", json!({"apiKey": "test"})),
        ("vercel-ai-gateway/chat", "openai/gpt-5.4", json!({"apiKey": "test"})),
        ("amazon-bedrock/mantle/responses", "openai.gpt-5.4", json!({"apiKey": "test", "region": "us-east-1"})),
        ("amazon-bedrock/mantle/chat", "openai.gpt-5.4", json!({"apiKey": "test", "region": "us-east-1"})),
        ("xai", "grok-4", json!({"apiKey": "test"})),
        ("anthropic", "claude-sonnet-4-5", json!({"apiKey": "test"})),
    ];
    for (name, model, settings) in routes {
        let case = |request: Value| json!({"package": name, "model": model, "settings": settings, "request": request});
        for format in [&text, &regex, &lark] {
            cases.push(case(json!({"prompt": "Run it.", "tools": [tool(format.clone()), other.clone()], "toolChoice": "run"})));
        }
        cases.push(case(json!({"prompt": "Run it.", "tools": [tool(Value::Null), other.clone()], "toolChoice": "run"})));
        cases.push(case(json!({"prompt": "Run it.", "tools": [tool(lark.clone())], "toolChoice": "lookup", "providerOptions": {"allowedTools": {"mode": "required", "toolNames": ["run", "lookup"]}}})));
        // A freeform declaration without exactly one required string parameter, and on a native tool.
        let mut bad = other.clone();
        bad["freeform"] = text.clone();
        cases.push(case(json!({"prompt": "Run it.", "tools": [bad]})));
        for metadata in [json!({"itemId": "ctc_1", "custom": true}), json!({"itemId": "fc_1"}), json!({})] {
            for declared in [lark.clone(), Value::Null] {
                cases.push(case(json!({"messages": history(metadata.clone()), "tools": [tool(declared), other.clone()]})));
            }
        }
        // A past call whose field is not a string replays as the JSON input.
        let mut odd = history(json!({"itemId": "ctc_1", "custom": true}));
        odd[1]["content"][1]["input"] = json!({"code": 5});
        cases.push(case(json!({"messages": odd, "tools": [tool(text.clone())]})));
    }
    cases
}

/// Compiles freeform requests on every route that reuses the OpenAI wire format (and Anthropic, which ignores
/// `freeform`) with upstream 2.3.0's routes and with ocd, and compares the URL and the body text (key order
/// included). Needs `OCD_SCRATCH` and `OCD_BUN_PATH`; skipped otherwise.
#[tokio::test]
async fn request_bodies_match_upstream_2_3_0() {
    let Some((ai, bun)) = upstream() else { return };
    let cases = compile_cases();
    let mut ours = vec![];
    for case in &cases {
        let req = our_request(case);
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
        .map(|(i, (theirs, ours))| format!("case {i} ({} {})\n  upstream: {theirs}\n  ocd:      {ours}", cases[i]["package"], cases[i]["request"]))
        .collect();
    eprintln!("compared {} requests with upstream 2.3.0", theirs.len());
    assert!(differing.is_empty(), "{} of {} differ:\n{}", differing.len(), cases.len(), differing.join("\n"));
}

const DECODE_SCRIPT: &str = r#"import { Effect, Stream } from "effect"
import { readFileSync, writeFileSync } from "node:fs"
const ai = $AI
const { ai: AI, ToolDefinition } = await import(ai + "/src/effect.ts")
const { LLMClient } = await import(ai + "/src/route.ts")
const { fixedResponse } = await import(ai + "/test/lib/http.ts")
const jobs = JSON.parse(readFileSync(process.argv[2], "utf8"))
const out = []
for (const job of jobs) {
  const pkg = await import(ai + "/src/providers/" + job.package + ".ts")
  const model = pkg.model(job.model, job.settings)
  const body = Buffer.from(job.body, "base64")
  const events = []
  const tools = job.tools.map((t) => ToolDefinition.make(t))
  const exit = await Effect.runPromiseExit(
    LLMClient.stream(AI.request({ model, prompt: "x", tools })).pipe(
      Stream.runForEach((event) => Effect.sync(() => events.push(event))),
      Effect.provide(fixedResponse(body, { headers: { "content-type": "text/event-stream" } })),
    ),
  )
  if (exit._tag === "Failure") {
    const error = exit.cause.reasons?.[0]?.error
    events.push({ error: error?.reason?._tag ?? "Defect", message: error?.reason?.message ?? String(exit.cause) })
  }
  out.push(JSON.parse(JSON.stringify(events)))
}
writeFileSync(process.argv[3], JSON.stringify(out))
"#;

fn decode_jobs() -> Vec<(String, Value, Vec<u8>)> {
    let tool = |name: &str, field: &str, freeform: Value| {
        let mut t = json!({"name": name, "description": "d", "inputSchema": {"type": "object", "properties": {field: {"type": "string"}}, "required": [field]}});
        if !freeform.is_null() {
            t["freeform"] = freeform;
        }
        t
    };
    let declared = vec![tool("run", "code", json!({"type": "text"})), tool("patch", "patchText", json!({"type": "lark", "grammar": GRAMMAR}))];
    let undeclared: Vec<Value> = vec![];
    let plain = vec![tool("run", "code", Value::Null)];
    let item = |id: &str, call: &str, name: &str, input: &str| json!({"type": "custom_tool_call", "id": id, "call_id": call, "name": name, "input": input});
    let added = |index: i64, item: Value| json!({"type": "response.output_item.added", "output_index": index, "item": item});
    let done = |index: i64, item: Value| json!({"type": "response.output_item.done", "output_index": index, "item": item});
    let delta = |index: i64, id: &str, text: &str| json!({"type": "response.custom_tool_call_input.delta", "output_index": index, "item_id": id, "delta": text});
    let completed = |output: Value| json!({"type": "response.completed", "response": {"id": "resp_1", "output": output, "usage": {"input_tokens": 3, "output_tokens": 2}}});
    let raw = "*** Begin Patch\n*** Add File: a.txt\n+hi \"there\"\t\\\n*** End Patch";
    let mut streams: Vec<(&str, Vec<Value>)> = vec![];
    // Streamed, then confirmed.
    streams.push(("streamed", vec![
        added(0, item("ctc_1", "call_1", "run", "")),
        delta(0, "ctc_1", "console.log("),
        delta(0, "ctc_1", "\"hi\")\n"),
        delta(0, "ctc_1", ""),
        json!({"type": "response.custom_tool_call_input.done", "output_index": 0, "item_id": "ctc_1", "input": "console.log(\"hi\")\n"}),
        done(0, item("ctc_1", "call_1", "run", "console.log(\"hi\")\n")),
        added(1, item("ctc_2", "call_2", "patch", "*** Begin")),
        delta(1, "ctc_2", &raw["*** Begin".len()..]),
        done(1, item("ctc_2", "call_2", "patch", raw)),
        completed(json!([])),
    ]));
    // The done item disagrees with the streamed text: the final input wins.
    streams.push(("disagrees", vec![
        added(0, item("ctc_1", "call_1", "run", "")),
        delta(0, "ctc_1", "partial"),
        done(0, item("ctc_1", "call_1", "run", "different")),
        completed(json!([])),
    ]));
    // Done only, and recovered from `response.completed` without a done event.
    streams.push(("done-only", vec![done(0, item("ctc_1", "call_1", "run", "x = 1")), completed(json!([]))]));
    streams.push(("recovered", vec![
        added(0, item("ctc_1", "call_1", "run", "")),
        delta(0, "ctc_1", "a"),
        completed(json!([item("ctc_1", "call_1", "run", "ab")])),
    ]));
    streams.push(("never-closed", vec![added(0, item("ctc_1", "call_1", "run", "")), delta(0, "ctc_1", "a\u{2028}😀"), completed(json!([]))]));
    // A delta without an item id is an error.
    streams.push(("missing-item", vec![
        added(0, item("ctc_1", "call_1", "run", "")),
        json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "delta": "x"}),
    ]));
    let responses = |frames: &[Value]| frames.iter().map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap_or(""))).collect::<String>().into_bytes();
    let mut jobs = vec![];
    for (label, frames) in &streams {
        for (tools_label, tools) in [("declared", &declared), ("undeclared", &undeclared), ("plain", &plain)] {
            jobs.push((format!("responses/{label}/{tools_label}"), json!({"package": "openai", "model": "gpt-5.4", "tools": tools}), responses(frames)));
        }
    }
    // Chat custom calls: streamed in pieces, two parallel calls (one function), a late id, and no finish chunk.
    let chunk = |calls: Value, finish: Value| json!({"id": "chatcmpl_1", "choices": [{"index": 0, "delta": {"tool_calls": calls}, "finish_reason": finish}]});
    let chat_streams: Vec<(&str, Vec<Value>)> = vec![
        ("streamed", vec![
            chunk(json!([{"index": 0, "id": "call_1", "type": "custom", "custom": {"name": "run", "input": "con"}}]), Value::Null),
            chunk(json!([{"index": 0, "custom": {"input": "sole(\"x\")\n"}}]), Value::Null),
            chunk(json!([{"index": 1, "id": "call_2", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":"}}]), Value::Null),
            chunk(json!([{"index": 1, "function": {"arguments": "\"y\"}"}}]), Value::Null),
            chunk(json!([{"index": 2, "id": "call_3", "type": "custom", "custom": {"name": "patch", "input": raw}}]), Value::Null),
            json!({"id": "chatcmpl_1", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]),
        ("late-id", vec![
            chunk(json!([{"index": 0, "type": "custom", "custom": {"name": "run", "input": "a"}}]), Value::Null),
            chunk(json!([{"index": 0, "id": "call_1", "custom": {"input": "b"}}]), Value::Null),
            json!({"id": "chatcmpl_1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ]),
        ("no-finish", vec![chunk(json!([{"index": 0, "id": "call_1", "type": "custom", "custom": {"name": "run", "input": "a"}}]), Value::Null)]),
    ];
    for (label, frames) in &chat_streams {
        let mut frames = frames.clone();
        frames.push(json!({"id": "chatcmpl_1", "choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}));
        for (tools_label, tools) in [("declared", &declared), ("undeclared", &undeclared)] {
            jobs.push((format!("chat/{label}/{tools_label}"), json!({"package": "openai/chat", "model": "gpt-5.4", "tools": tools}), sse(&frames)));
            jobs.push((
                format!("compatible-chat/{label}/{tools_label}"),
                json!({"package": "openai-compatible", "model": "gpt-5.4", "settings": {"baseURL": "https://chat.test/v1"}, "tools": tools}),
                sse(&frames),
            ));
        }
    }
    // The recorded freeform calls (OpenRouter, DigitalOcean and Venice Responses).
    let root = format!("{}/tests/fixtures/recorded", env!("CARGO_MANIFEST_DIR"));
    for (dir, package_name, model) in [
        ("openrouter-responses", "openrouter/responses", "openai/gpt-5.4-mini"),
        ("digitalocean-responses", "digitalocean/responses", "openai-gpt-5.4-mini"),
        ("venice-responses", "venice/responses", "openai-gpt-54-mini"),
    ] {
        let rec = oc_ai::testing::recording(&std::fs::read_to_string(format!("{root}/{dir}/calls-a-freeform-text-tool.json")).unwrap());
        let run = json!([{"name": "run", "description": "Run a Python snippet.", "inputSchema": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"], "additionalProperties": false}, "freeform": {"type": "text"}}]);
        jobs.push((format!("recorded/{dir}"), json!({"package": package_name, "model": model, "tools": run}), rec.interactions[0].response.bytes()));
    }
    jobs.into_iter()
        .map(|(label, mut job, body)| {
            if job.get("settings").is_none() {
                job["settings"] = json!({"apiKey": "fixture"});
            } else {
                job["settings"]["apiKey"] = json!("fixture");
            }
            (label, job, body)
        })
        .collect()
}

/// Decodes custom tool call streams (Responses and Chat, with the tools declared freeform, not declared, and declared
/// as JSON tools) and the recorded freeform calls with upstream 2.3.0's own routes and with ocd, and compares the
/// events. Needs `OCD_SCRATCH` and `OCD_BUN_PATH`; skipped otherwise.
#[test]
fn custom_tool_calls_decode_like_upstream_2_3_0() {
    let Some((ai, bun)) = upstream() else { return };
    let jobs = decode_jobs();
    let mut ours = vec![];
    let mut input = vec![];
    for (_, job, body) in &jobs {
        let model = package(job["package"].as_str().unwrap(), job["model"].as_str().unwrap(), job["settings"].clone());
        let mut req = LLMRequest::new(model).prompt("x");
        for tool in job["tools"].as_array().unwrap() {
            req.tools.push(ToolEntry::Tool(serde_json::from_value(tool.clone()).unwrap()));
        }
        let events: Vec<Value> = decode(&req, body)
            .into_iter()
            .map(|e| match e {
                Ok(event) => serde_json::to_value(event).unwrap(),
                Err(error) => json!({"error": error.tag(), "message": error.message}),
            })
            .collect();
        ours.push(numbers(Value::Array(events)));
        let mut job = job.clone();
        job["body"] = json!(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, body));
        input.push(job);
    }
    let theirs = run_bun(&ai, &bun, DECODE_SCRIPT, &Value::Array(input));
    let theirs = theirs.as_array().unwrap();
    assert_eq!(theirs.len(), ours.len());
    let differing: Vec<String> = theirs
        .iter()
        .zip(ours.iter())
        .enumerate()
        .filter(|(_, (theirs, ours))| numbers((*theirs).clone()) != **ours)
        .map(|(i, (theirs, ours))| format!("{}\n  upstream: {}\n  ocd:      {}", jobs[i].0, numbers(theirs.clone()), ours))
        .collect();
    eprintln!("compared {} custom tool call streams with upstream 2.3.0", ours.len());
    assert!(differing.is_empty(), "{} differ:\n{}", differing.len(), differing.join("\n"));
}

#[allow(dead_code)]
fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}
