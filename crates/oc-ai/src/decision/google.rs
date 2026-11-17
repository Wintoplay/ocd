//! `protocols/google-decisions.ts` (2.3.0, #63369): Google's `/decisions`, which takes System One's questions and
//! answers with a content-block input.

use super::system_one::{encode_questions, to_answer, valid_answer};
use super::{identity, DecisionPart, DecisionProtocol, DecisionRequest, DecisionResponse, DecisionRoute, DecisionUsage};
use crate::error::AIError;
use crate::json::{encode, merge_records, Object};
use crate::media::{MediaAsset, MediaKind, MediaSource};
use crate::route::media::{invalid_request, total_tokens, MediaEndpoint, MediaResponse};
use serde_json::{json, Value};
use std::sync::Arc;

pub const ID: &str = "google-decisions";
const NAME: &str = "Google Decisions";
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const PATH: &str = "/decisions";

/// `Options`: `mediaResolution` is the token budget for every image and video input (`ultra_high` applies to
/// images only).
fn decode_options(options: &Object) -> Result<Option<String>, AIError> {
    match options.get("mediaResolution") {
        None => Ok(None),
        Some(Value::String(resolution)) => Ok(Some(resolution.clone())),
        Some(_) => Err(invalid_request("Expected string, got non-string at [\"mediaResolution\"]")),
    }
}

fn input_part(request: &DecisionRequest, part: &DecisionPart, resolution: Option<&str>) -> Result<Object, AIError> {
    match part {
        DecisionPart::Text(text) => Ok(json!({ "type": "text", "text": text }).as_object().cloned().unwrap()),
        // Google reads every text block as text; JSON is serialized so its keys are never parsed as content blocks.
        DecisionPart::Json(value) => Ok(json!({ "type": "text", "text": encode(value) }).as_object().cloned().unwrap()),
        DecisionPart::Media { media, .. } => media_part(request, media, resolution),
    }
}

/// Google joins adjacent text blocks with no separator, so each one after another starts on a new line.
fn separate_text(blocks: Vec<Object>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(blocks.len());
    let mut previous_text = false;
    for mut block in blocks {
        let text = block.get("type").and_then(Value::as_str) == Some("text");
        if text && previous_text {
            let joined = format!("\n{}", block.get("text").and_then(Value::as_str).unwrap_or_default());
            block.insert("text".into(), json!(joined));
        }
        previous_text = text;
        out.push(Value::Object(block));
    }
    out
}

fn media_part(request: &DecisionRequest, asset: &MediaAsset, resolution: Option<&str>) -> Result<Object, AIError> {
    let media_type = asset.media_type().to_lowercase();
    let kind = match asset.kind() {
        MediaKind::Image => Some("image"),
        MediaKind::Audio => Some("audio"),
        MediaKind::Video => Some("video"),
        _ if media_type == "application/pdf" => Some("document"),
        _ => None,
    };
    let Some(kind) = kind else {
        return Err(AIError::unsupported(
            "decision.media",
            format!("{NAME} takes images, audio, video and PDFs; got {}", asset.media_type()),
            Some(&request.model.provider),
            Some(&request.model.route.id),
        ));
    };
    let mut block = Object::new();
    block.insert("type".into(), json!(kind));
    block.insert("mime_type".into(), json!(media_type));
    // `ultra_high` is image-only, so video keeps Google's default for it.
    if let Some(resolution) = resolution
        && (kind == "image" || (kind == "video" && resolution != "ultra_high"))
    {
        block.insert("resolution".into(), json!(resolution));
    }
    if let Some(inline) = asset.inline() {
        block.insert("data".into(), json!(inline.base64));
        return Ok(block);
    }
    // Google fetches URLs itself.
    if let MediaSource::Url { url, .. } = &asset.source
        && url.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
    {
        block.insert("uri".into(), json!(url));
        return Ok(block);
    }
    if let MediaSource::Ref { provider, id, .. } = &asset.source
        && provider == "google"
    {
        block.insert("uri".into(), json!(id));
        return Ok(block);
    }
    Err(invalid_request(format!(
        "{NAME} accepts inline media, public HTTPS URLs and Google Files API URIs; materialize {} media first",
        asset.source_type()
    )))
}

/// A lenient-shaped `Usage`: the known counts must be numbers; other keys are kept as they are.
fn valid_usage(usage: &Value) -> bool {
    usage.as_object().is_some_and(|u| {
        ["total_input_tokens", "total_output_tokens", "total_tokens"].iter().all(|k| u.get(*k).is_none_or(Value::is_number))
    })
}

pub struct GoogleDecisions;

impl DecisionProtocol for GoogleDecisions {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        NAME
    }

    fn body(&self, request: &DecisionRequest) -> Result<Object, AIError> {
        let options = request.provider_options.clone().unwrap_or_default();
        let resolution = decode_options(&options)?;
        let mut native = options;
        native.shift_remove("mediaResolution");
        let mut out = merge_records(&[Some(&native), request.http.as_ref().and_then(|h| h.body.as_ref())]).unwrap_or_default();
        let mut blocks = vec![];
        for part in &request.input {
            blocks.push(input_part(request, part, resolution.as_deref())?);
        }
        let mut questions = encode_questions(&request.questions)?;
        // Google names System One's predicate criteria `true_description` and `false_description`.
        for question in &request.questions {
            if question.get("type").and_then(Value::as_str) != Some("predicate") {
                continue;
            }
            let Some(criteria) = question.get("criteria") else { continue };
            let mut renamed = Object::new();
            for (from, to) in [("true", "true_description"), ("false", "false_description")] {
                if let Some(value) = criteria.get(from) {
                    renamed.insert(to.into(), value.clone());
                }
            }
            let name = question.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            questions.insert(
                name,
                json!({ "type": "noul", "instructions": question.get("instructions").cloned().unwrap_or(Value::Null), "criteria": renamed }),
            );
        }
        out.insert("model".into(), json!(request.model.id));
        out.insert("input".into(), Value::Array(separate_text(blocks)));
        out.insert("questions".into(), Value::Object(questions));
        Ok(out)
    }

    fn decode(&self, response: &MediaResponse, request: &DecisionRequest, metadata_key: &str) -> Result<DecisionResponse, AIError> {
        let ident = identity(ID, NAME, "google");
        let invalid = |message: String| ident.invalid(response, message);
        let value: Value = serde_json::from_slice(&response.body).map_err(|_| invalid(format!("{NAME} returned an invalid response")))?;
        let valid = value.get("answers").and_then(Value::as_object).is_some_and(|a| a.values().all(valid_answer))
            && value.get("usage").is_none_or(valid_usage);
        if !valid {
            return Err(invalid(format!("{NAME} returned an invalid response")));
        }
        let answers = value["answers"].as_object().unwrap();
        let names: Vec<&str> = request.questions.iter().filter_map(|q| q.get("name").and_then(Value::as_str)).collect();
        if answers.len() != names.len() || !names.iter().all(|n| answers.contains_key(*n)) {
            return Err(invalid(format!("{NAME} answers do not match the requested questions")));
        }
        let mapped: Vec<Object> = request
            .questions
            .iter()
            .map(|q| {
                let name = q.get("name").and_then(Value::as_str).unwrap_or_default();
                to_answer(q, answers[name].as_object().unwrap())
            })
            .collect();
        let usage_raw = value.get("usage").and_then(Value::as_object);
        let usage = usage_raw.map(|u| {
            let input = u.get("total_input_tokens").and_then(Value::as_f64);
            let output = u.get("total_output_tokens").and_then(Value::as_f64);
            DecisionUsage { input_tokens: input, output_tokens: output, total_tokens: total_tokens(input, output, u.get("total_tokens").and_then(Value::as_f64)), cost: None }
        });
        let provider_metadata = usage_raw.map(|u| {
            let mut metadata = Object::new();
            metadata.insert(metadata_key.into(), json!({ "usage": u }));
            metadata
        });
        Ok(DecisionResponse { answers: mapped, usage, provider_metadata })
    }
}

/// `GoogleDecisions.route`.
pub fn route() -> DecisionRoute {
    DecisionRoute::new(ID, "google", Arc::new(GoogleDecisions), MediaEndpoint::new(Some(DEFAULT_BASE_URL), PATH))
}
