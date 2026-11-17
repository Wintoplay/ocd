//! Freeform tools as OpenAI custom tools, shared by Responses and Chat Completions (`protocols/openai-freeform.ts`):
//! <https://platform.openai.com/docs/guides/function-calling#custom-tools>. Calls keep the normal `{ field: raw }`
//! input everywhere else; only the wire carries raw text.

use super::shared::flatten_tools;
use super::tool_stream::{StreamKey, ToolStream};
use crate::json::{encode, Object};
use crate::types::{Freeform, LLMEvent, LLMRequest, ProviderMetadata, ToolDefinition};
use indexmap::IndexMap;
use serde_json::{json, Value};

/// A tool that goes out as a custom tool.
#[derive(Debug, Clone, PartialEq)]
pub struct Custom {
    /// The tool's one required string parameter, which the raw text fills.
    pub field: String,
    pub format: Freeform,
}

/// How a tool goes out: a custom tool when it declares `freeform` with one required string parameter.
pub fn resolve(tool: &ToolDefinition) -> Option<Custom> {
    let field = ToolDefinition::freeform_field(&tool.input_schema);
    match (&tool.freeform, &tool.native, field) {
        (Some(format), None, Some(field)) => Some(Custom { field, format: format.clone() }),
        _ => None,
    }
}

/// Custom tools in this request, by flattened tool name.
pub fn tools(request: &LLMRequest) -> IndexMap<String, Custom> {
    flatten_tools(&request.tools).iter().filter_map(|tool| resolve(tool).map(|custom| (tool.name.clone(), custom))).collect()
}

/// The format's grammar (`{syntax, definition}`), or `None` for unconstrained text.
pub fn grammar(format: &Freeform) -> Option<Object> {
    let (syntax, definition) = match format {
        Freeform::Text => return None,
        Freeform::Regex { pattern } => ("regex", pattern),
        Freeform::Lark { grammar } => ("lark", grammar),
    };
    let mut o = Object::new();
    o.insert("syntax".into(), json!(syntax));
    o.insert("definition".into(), json!(definition));
    Some(o)
}

/// Raw text replayed for a past call: its field, or the whole input as JSON when the field is not a string.
pub fn to_raw(custom: &Custom, input: &Value) -> String {
    match input.as_object().and_then(|o| o.get(&custom.field)) {
        Some(Value::String(s)) => s.clone(),
        _ if input.is_null() => encode(&Value::Object(Object::new())),
        _ => encode(input),
    }
}

fn field(custom: Option<&Custom>) -> &str {
    custom.map(|c| c.field.as_str()).unwrap_or("input")
}

/// JSON arguments for a raw call. A call to an undeclared custom tool fills `input`.
pub fn from_raw(custom: Option<&Custom>, raw: &str) -> String {
    let mut o = Object::new();
    o.insert(field(custom).into(), json!(raw));
    encode(&Value::Object(o))
}

// Streamed raw text is re-encoded as JSON-argument fragments, so the tool-input events match a JSON tool's.

/// The opening fragment, `{"field":"`, followed by the escaped first text.
pub fn stream_start(custom: Option<&Custom>, raw: &str) -> String {
    format!("{{{}:\"{}", encode(&json!(field(custom))), stream_delta(raw))
}

/// One raw chunk as the inside of a JSON string.
pub fn stream_delta(raw: &str) -> String {
    let encoded = encode(&json!(raw));
    encoded[1..encoded.len() - 1].to_string()
}

const STREAM_END: &str = "\"}";

/// Close the JSON fragment of each open custom call. With a `final` raw input, a call only closes when its fragments
/// decode to it, so an authoritative final input that disagrees replaces the streamed one instead.
pub fn close(tools: &mut ToolStream, keys: &[StreamKey], final_input: Option<(Option<&Custom>, &str)>, events: &mut Vec<LLMEvent>) {
    for key in keys {
        let Some(pending) = tools.get(key) else { continue };
        if let Some((custom, raw)) = final_input {
            let streamed = serde_json::from_str::<Value>(&format!("{}{STREAM_END}", pending.input)).ok().filter(Value::is_object);
            let value = streamed.as_ref().and_then(|s| s.get(field(custom)));
            if value.and_then(Value::as_str) != Some(raw) {
                continue;
            }
        }
        tools.append(key.clone(), STREAM_END, events);
    }
}

/// Provider metadata marking a call that arrived as a custom tool call.
pub fn metadata(item_id: &str, custom: bool) -> Value {
    if custom { json!({"itemId": item_id, "custom": true}) } else { json!({"itemId": item_id}) }
}

/// Whether a replayed call keeps its item id: only when it goes out as the type it arrived as, since OpenAI item ids
/// name their type (`fc_`, `ctc_`). The `call_id` always survives.
pub fn keeps_item_id(provider_metadata: Option<&ProviderMetadata>, key: &str, custom: bool) -> bool {
    let arrived_custom = provider_metadata.and_then(|pm| pm.get(key)).and_then(|m| m.get("custom")) == Some(&Value::Bool(true));
    arrived_custom == custom
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::tool_stream::PendingTool;

    fn custom() -> Custom {
        Custom { field: "code".into(), format: Freeform::Text }
    }

    fn input_schema() -> Value {
        json!({"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"], "additionalProperties": false})
    }

    fn tool(f: impl FnOnce(&mut ToolDefinition)) -> ToolDefinition {
        let mut t = ToolDefinition::new("run", "Run code", input_schema()).with_freeform(Some(Freeform::Text));
        f(&mut t);
        t
    }

    const RAW: [&str; 3] = ["say \"hi\"\n\tdone\\", "naïve 😀 \u{2028}", "\u{0000} }{ ]"];

    /// openai-freeform.test.ts "resolves a freeform tool to its string field".
    #[test]
    fn resolves_a_freeform_tool_to_its_string_field() {
        assert_eq!(resolve(&tool(|_| {})), Some(custom()));
        assert_eq!(resolve(&tool(|t| t.freeform = None)), None);
        assert_eq!(resolve(&tool(|t| t.native = Some(json!({"openai": {}}).as_object().cloned().unwrap()))), None);
        assert_eq!(resolve(&tool(|t| t.input_schema["properties"] = json!({"code": {"type": "number"}}))), None);
        assert_eq!(resolve(&tool(|t| t.input_schema["required"] = json!([]))), None);
    }

    /// openai-freeform.test.ts "converts between raw text and the JSON input".
    #[test]
    fn converts_between_raw_text_and_the_json_input() {
        for raw in RAW {
            let encoded = from_raw(Some(&custom()), raw);
            let parsed: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(parsed, json!({"code": raw}));
            assert_eq!(to_raw(&custom(), &parsed), raw);
        }
        assert_eq!(from_raw(None, "x"), r#"{"input":"x"}"#);
        assert_eq!(to_raw(&custom(), &json!({"code": 1})), r#"{"code":1}"#);
    }

    /// openai-freeform.test.ts "streams raw chunks as JSON fragments that add up to the input" (split at every
    /// character; Rust strings hold no lone surrogates).
    #[test]
    fn streams_raw_chunks_as_json_fragments_that_add_up_to_the_input() {
        for raw in RAW {
            let mut fragments = vec![stream_start(Some(&custom()), "")];
            fragments.extend(raw.chars().map(|c| stream_delta(&c.to_string())));
            let mut tools = ToolStream::new("test");
            tools.start(StreamKey::from("ctc_1"), PendingTool { input: fragments.concat(), ..PendingTool::new("call_1", "run") });
            let mut events = vec![];
            close(&mut tools, &[StreamKey::from("ctc_1")], Some((Some(&custom()), raw)), &mut events);
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0], LLMEvent::ToolInputDelta { text, .. } if text == "\"}"));
            let input: Value = serde_json::from_str(&tools.get(&StreamKey::from("ctc_1")).unwrap().input).unwrap();
            assert_eq!(input, json!({"code": raw}));
        }
    }

    /// openai-freeform.test.ts "leaves a call open when the final input disagrees with the streamed one".
    #[test]
    fn leaves_a_call_open_when_the_final_input_disagrees() {
        let mut tools = ToolStream::new("test");
        tools.start(StreamKey::from("ctc_1"), PendingTool { input: stream_start(Some(&custom()), "partial"), ..PendingTool::new("call_1", "run") });
        let before = tools.get(&StreamKey::from("ctc_1")).cloned();
        let mut events = vec![];
        close(&mut tools, &[StreamKey::from("ctc_1"), StreamKey::from("missing")], Some((Some(&custom()), "different")), &mut events);
        assert!(events.is_empty());
        assert_eq!(tools.get(&StreamKey::from("ctc_1")).cloned(), before);
        close(&mut tools, &[StreamKey::from("ctc_1")], None, &mut events);
        assert_eq!(events.iter().filter(|e| matches!(e, LLMEvent::ToolInputDelta { .. })).count(), 1);
    }

    /// openai-freeform.test.ts "keeps an item id only while the call type is unchanged".
    #[test]
    fn keeps_an_item_id_only_while_the_call_type_is_unchanged() {
        let pm = |value: Value| json!({"openai": value}).as_object().cloned().unwrap();
        let custom_call = pm(metadata("ctc_1", true));
        let function_call = pm(metadata("fc_1", false));
        assert!(keeps_item_id(Some(&custom_call), "openai", true));
        assert!(!keeps_item_id(Some(&custom_call), "openai", false));
        assert!(keeps_item_id(Some(&function_call), "openai", false));
        assert!(!keeps_item_id(Some(&function_call), "openai", true));
        assert!(keeps_item_id(None, "openai", false));
    }
}
