//! The `execute` tool as the agent sees it: name, description, input schema, result formatting, and progress
//! rows. Port of `packages/core/src/codemode/tool.ts` (the parts that do not depend on core's tool registry).

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value as Json};

use crate::codemode::ExecResult;
use crate::host::{CallResult, ExtensionInvocation, HostValue, ToolInvocation};

pub const NAME: &str = "execute";

/// Invariant model-facing guidance; the changing tool catalog is delivered through the catalog instructions.
pub const DESCRIPTION: &str = concat!(
    "Run JavaScript in a confined Code Mode runtime to script tool calls and HTTP requests and compose their results.\n",
    "Write plain JavaScript and send it exactly as written: no code fence, and no JSON or other wrapper around it.\n",
    "`fetch` is available for HTTP requests. Imports, direct filesystem access, and timers are unavailable: there is no `setTimeout` or sleep. All other external access goes through `tools`.\n",
    "Inside `execute`, the only callable tools are those explicitly listed in the Code Mode catalog instructions or returned by the `search` function. Ignore tools shown outside the Code Mode catalog. They are not available in the Code Mode runtime.\n",
    "Call tools through `tools` using only exact paths and signatures from the catalog. Do not infer or normalize tool names; preserve bracket notation such as `tools.<namespace>[\"tool-name\"](input)`.\n",
    "Prefer an explicit `return`; if omitted, the final top-level expression becomes the result.\n",
    "Await every call whose completion matters; pending calls are interrupted when execution ends. Run independent calls concurrently with `Promise.allSettled`."
);

/// `{ code: string }` (`CodeMode.Input`).
pub fn input_schema() -> Json {
    json!({
        "type": "object",
        "properties": { "code": { "type": "string" } },
        "required": ["code"],
        "additionalProperties": false
    })
}

/// A file part collected from nested tool results (`data:` URIs), re-emitted after the text output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecuteFile {
    pub data: String,
    pub mime: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallStatus {
    Running,
    Completed,
    Error,
}

/// One progress row: a nested tool call (or a listed extension call such as `fetch`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecuteCall {
    pub tool: String,
    pub status: CallStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input: Option<Map<String, Json>>,
}

/// The `execute` tool's structured output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecuteOutput {
    pub output: String,
    #[serde(rename = "toolCalls")]
    pub tool_calls: Vec<ExecuteCall>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<bool>,
    pub files: Vec<ExecuteFile>,
}

// Compact JSON: indentation adds tokens on every later step without helping the model. Clients pretty-print it
// (`format_execute_output`).
fn format_value(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => crate::data::stringify(other, ""),
    }
}

/// `formatExecuteOutput` (`@opencode/util/execute-output`): an `execute` output for display. The result (compact
/// JSON for structured values) is followed by optional "Warnings:" and "Logs:" sections; a JSON result comes back
/// indented as `json` (as `JSON.stringify(JSON.parse(result), null, 2)` prints it), the sections as `rest`, and
/// `text` joins them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormattedOutput {
    pub json: Option<String>,
    pub rest: Option<String>,
    pub text: String,
}

pub fn format_execute_output(output: &str) -> FormattedOutput {
    let end = ["\n\nWarnings:\n", "\n\nLogs:\n"].iter().filter_map(|m| output.find(m)).min();
    let result = end.map_or(output, |end| &output[..end]);
    let rest = end.map(|end| {
        output[end..].trim_matches(|c: char| (c as u32) < 0x10000 && crate::number::is_js_whitespace(c as u16)).to_string()
    });
    let json = indented_json(result);
    let shown = json.as_deref().unwrap_or(result);
    let text = match &rest {
        None => shown.to_string(),
        Some(rest) => format!("{shown}\n\n{rest}"),
    };
    FormattedOutput { json, rest, text }
}

fn indented_json(text: &str) -> Option<String> {
    if !text.starts_with('{') && !text.starts_with('[') {
        return None;
    }
    let parsed = crate::stdlib::json::parse_json(&crate::value::JsStr::from(text)).ok()?;
    Some(crate::data::stringify(&parsed_json(&parsed), "  "))
}

/// A parsed JSON value as host JSON with a JS object's key order (integer-like keys first, a repeated key keeping
/// its first place and last value); lone surrogates stay marked so they print as `\udXXX` escapes.
fn parsed_json(parsed: &crate::stdlib::json::Parsed) -> Json {
    use crate::stdlib::json::Parsed;
    match parsed {
        Parsed::Null => Json::Null,
        Parsed::Bool(b) => Json::Bool(*b),
        Parsed::Number(n) => serde_json::Number::from_f64(*n).map(Json::Number).unwrap_or(Json::Null),
        Parsed::String(s) => Json::String(crate::data::marked_string(s.units())),
        Parsed::Array(items) => Json::Array(items.iter().map(parsed_json).collect()),
        Parsed::Object(entries) => {
            let mut unique: indexmap::IndexMap<String, Json> = indexmap::IndexMap::new();
            for (key, value) in entries {
                unique.insert(crate::data::marked_string(key.units()), parsed_json(value));
            }
            Json::Object(crate::data::js_ordered(unique.into_iter().collect()))
        }
    }
}

/// The text output of an execution: the value (strings verbatim, else compact JSON) or the error message plus
/// suggestions not already contained, then `Warnings:` and `Logs:` sections.
pub fn format_result(result: &ExecResult) -> String {
    let output = match result {
        ExecResult::Success { value, .. } => format_value(value),
        ExecResult::Failure { error, .. } => {
            let mut parts = vec![error.message.clone()];
            for hint in error.suggestions.iter().flatten() {
                if !error.message.contains(hint.as_str()) {
                    parts.push(hint.clone());
                }
            }
            js_trim(&parts.join("\n")).to_string()
        }
    };
    let warnings = match result {
        ExecResult::Success { warnings: Some(w), .. } if !w.is_empty() => Some(format!(
            "Warnings:\n{}",
            w.iter().map(|d| format!("- [{}] {}", d.kind.as_str(), d.message)).collect::<Vec<_>>().join("\n")
        )),
        _ => None,
    };
    let logs = match result.logs() {
        Some(l) if !l.is_empty() => Some(format!("Logs:\n{}", l.join("\n"))),
        _ => None,
    };
    // A program that only logs returns undefined (sent as null); a "null" line before its logs says nothing (2.3.1).
    let output = match result {
        ExecResult::Success { value: Json::Null, .. } if warnings.is_some() || logs.is_some() => None,
        _ => Some(output),
    };
    [output, warnings, logs].into_iter().flatten().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c as u32) < 0x10000 && crate::number::is_js_whitespace(c as u16))
}

/// What a progress row shows for a tool input: objects as themselves (none when empty), other values wrapped as
/// `{ input }`, nothing for null/undefined.
pub fn display_input(input: Option<&Json>) -> Option<Map<String, Json>> {
    match input {
        None | Some(Json::Null) => None,
        Some(Json::Object(m)) => {
            if m.is_empty() {
                None
            } else {
                Some(m.clone())
            }
        }
        Some(other) => {
            let mut m = Map::new();
            m.insert("input".into(), other.clone());
            Some(m)
        }
    }
}

/// What to show for a `fetch` call: its method (uppercased, default GET) and URL.
pub fn fetch_display(args: &[HostValue]) -> Map<String, Json> {
    let url = match args.first() {
        Some(HostValue::String(s)) | Some(HostValue::Url(s)) => s.clone(),
        Some(HostValue::Undefined) | None => "undefined".into(),
        Some(HostValue::Null) => "null".into(),
        Some(HostValue::Number(n)) => crate::number::to_string(*n),
        Some(HostValue::Bool(b)) => b.to_string(),
        Some(_) => "[object Object]".into(),
    };
    let method = args
        .get(1)
        .and_then(|init| init.field("method"))
        .and_then(|m| m.as_str())
        .map(|m| m.to_uppercase())
        .unwrap_or_else(|| "GET".into());
    let mut out = Map::new();
    out.insert("method".into(), Json::String(method));
    out.insert("url".into(), Json::String(url));
    out
}

/// Progress rows in start order; hooks find their row again by call id. Hosts call these from their
/// `tool_before`/`tool_after`/`extension_before`/`extension_after` hooks and forward `rows()` as progress.
#[derive(Default)]
pub struct ProgressRows {
    inner: Mutex<(Vec<ExecuteCall>, Vec<(u64, usize)>)>,
}

impl ProgressRows {
    pub fn new() -> Self {
        Self::default()
    }
    fn start(&self, id: u64, entry: ExecuteCall) -> Vec<ExecuteCall> {
        let mut inner = self.inner.lock().unwrap();
        let index = inner.0.len();
        inner.1.push((id, index));
        inner.0.push(entry);
        inner.0.clone()
    }
    fn settle(&self, id: u64, result: &CallResult) -> Option<Vec<ExecuteCall>> {
        let mut inner = self.inner.lock().unwrap();
        let index = inner.1.iter().find(|(i, _)| *i == id).map(|(_, idx)| *idx)?;
        inner.0[index].status =
            if matches!(result, CallResult::Success(_)) { CallStatus::Completed } else { CallStatus::Error };
        Some(inner.0.clone())
    }
    /// `tool.before`: adds a running row; returns the rows to publish.
    pub fn tool_started(&self, call: &ToolInvocation) -> Vec<ExecuteCall> {
        self.start(
            call.id,
            ExecuteCall { tool: call.name.clone(), status: CallStatus::Running, input: display_input(call.input.as_ref()) },
        )
    }
    /// `tool.after` / `extension.after`: flips the row's status; `None` when the call had no row.
    pub fn settled(&self, id: u64, result: &CallResult) -> Option<Vec<ExecuteCall>> {
        self.settle(id, result)
    }
    /// The execution was interrupted: calls still running settle as errors, as their `tool.after` would see the
    /// interruption. Returns whether a row changed.
    pub fn interrupt_running(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let mut changed = false;
        for row in inner.0.iter_mut().filter(|row| row.status == CallStatus::Running) {
            row.status = CallStatus::Error;
            changed = true;
        }
        changed
    }
    /// `extension.before`: only listed extension functions (`fetch`) get a row.
    pub fn extension_started(&self, call: &ExtensionInvocation) -> Option<Vec<ExecuteCall>> {
        match call.name.as_str() {
            "fetch" => Some(self.start(
                call.id,
                ExecuteCall { tool: call.name.clone(), status: CallStatus::Running, input: Some(fetch_display(&call.args)) },
            )),
            _ => None,
        }
    }
    pub fn rows(&self) -> Vec<ExecuteCall> {
        self.inner.lock().unwrap().0.clone()
    }
}

/// Assembles the execute tool's output from an execution result, progress rows, and collected files.
pub fn output(result: &ExecResult, tool_calls: Vec<ExecuteCall>, files: Vec<ExecuteFile>) -> ExecuteOutput {
    ExecuteOutput {
        output: format_result(result),
        tool_calls,
        error: if result.ok() { None } else { Some(true) },
        files,
    }
}

/// Splits `data:<mime>;base64,<data>` file URIs out of a tool result's content (core's `outputFiles`).
pub fn output_file(uri: &str, mime: &str, name: Option<&str>) -> Option<ExecuteFile> {
    let prefix = format!("data:{mime};base64,");
    uri.strip_prefix(&prefix).map(|data| ExecuteFile {
        data: data.to_string(),
        mime: mime.to_string(),
        name: name.map(String::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::errors::{Diagnostic, DiagnosticKind};

    #[test]
    fn formats_success_and_failure() {
        let ok = ExecResult::Success {
            value: json!({"a": [1, 2]}),
            warnings: Some(vec![Diagnostic::new(DiagnosticKind::ExecutionFailure, "boom")]),
            logs: Some(vec!["hello".into()]),
            truncated: false,
            tool_calls: vec![],
        };
        assert_eq!(
            format_result(&ok),
            "{\"a\":[1,2]}\n\nWarnings:\n- [ExecutionFailure] boom\n\nLogs:\nhello"
        );
        let err = ExecResult::Failure {
            error: Diagnostic {
                kind: DiagnosticKind::UnknownTool,
                message: "Unknown tool 'x'.".into(),
                location: None,
                suggestions: Some(vec!["Use search to find available tools.".into()]),
            },
            logs: None,
            truncated: false,
            tool_calls: vec![],
        };
        assert_eq!(format_result(&err), "Unknown tool 'x'.\nUse search to find available tools.");
        let s = ExecResult::Success { value: json!("text"), warnings: None, logs: None, truncated: false, tool_calls: vec![] };
        assert_eq!(format_result(&s), "text");
    }

    // packages/util/src/execute-output.test.ts
    #[test]
    fn indents_a_json_result() {
        let indented = "{\n  \"a\": 1,\n  \"b\": [\n    2\n  ]\n}";
        assert_eq!(
            format_execute_output("{\"a\":1,\"b\":[2]}"),
            FormattedOutput { json: Some(indented.into()), rest: None, text: indented.into() }
        );
    }

    #[test]
    fn indents_a_json_result_followed_by_warnings_and_logs() {
        assert_eq!(
            format_execute_output("[\"x\"]\n\nWarnings:\n- [unhandled] boom\n\nLogs:\nhi"),
            FormattedOutput {
                json: Some("[\n  \"x\"\n]".into()),
                rest: Some("Warnings:\n- [unhandled] boom\n\nLogs:\nhi".into()),
                text: "[\n  \"x\"\n]\n\nWarnings:\n- [unhandled] boom\n\nLogs:\nhi".into(),
            }
        );
    }

    #[test]
    fn leaves_other_results_as_they_are() {
        assert_eq!(
            format_execute_output("exit 0\nok\n\nLogs:\nhi"),
            FormattedOutput { json: None, rest: Some("Logs:\nhi".into()), text: "exit 0\nok\n\nLogs:\nhi".into() }
        );
        assert_eq!(format_execute_output("{not json"), FormattedOutput { json: None, rest: None, text: "{not json".into() });
    }

    #[test]
    fn indents_like_javascript() {
        // JSON.parse then JSON.stringify(_, null, 2): integer keys first, a repeated key keeps its first place and
        // last value, numbers in JS form, lone surrogates escaped.
        let out = format_execute_output("{\"b\":1,\"2\":1e21,\"1\":1.50,\"b\":\"\\ud800\",\"e\":{},\"f\":[]}");
        assert_eq!(
            out.json.as_deref(),
            Some("{\n  \"1\": 1.5,\n  \"2\": 1e+21,\n  \"b\": \"\\ud800\",\n  \"e\": {},\n  \"f\": []\n}")
        );
    }

    #[test]
    fn display_inputs() {
        assert_eq!(display_input(Some(&json!({}))), None);
        assert_eq!(display_input(Some(&json!(5))).unwrap()["input"], json!(5));
        let d = fetch_display(&[HostValue::str("https://x"), HostValue::Object(vec![("method".into(), HostValue::str("post"))])]);
        assert_eq!(Json::Object(d), json!({"method": "POST", "url": "https://x"}));
    }
}
