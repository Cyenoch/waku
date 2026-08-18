//! OpenAI Responses wire adapter (`/v1/responses`).

use super::{AssistantScratch, PayloadOutcome};
use crate::error::HarnessError;
use crate::events::StreamEvent;
use crate::model::{
    ContentBlock, Message, PromptContext, RequestOptions, StopReason, ToolResultPart, UserPart,
};
use crate::transform::{normalize_tool_call_id, repaired_messages, short_hash};
use serde_json::{Value, json};

pub const FORMAT: &str = "openai-responses";
const MIN_MAX_OUTPUT_TOKENS: u64 = 16;

pub fn build_body(
    ctx: &PromptContext,
    model: &str,
    opts: &RequestOptions,
) -> Result<Value, HarnessError> {
    let mut input: Vec<Value> = Vec::new();
    if let Some(system) = &ctx.system_prompt {
        input.push(json!({ "role": "developer", "content": system }));
    }
    for msg in repaired_messages(&ctx.messages).iter() {
        match msg {
            Message::User(u) => {
                let content: Vec<Value> = u
                    .parts
                    .iter()
                    .map(|p| match p {
                        UserPart::Text(t) => json!({ "type": "input_text", "text": t }),
                        UserPart::Image {
                            mime_type,
                            data_b64,
                        } => json!({
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": format!("data:{mime_type};base64,{data_b64}"),
                        }),
                    })
                    .collect();
                if !content.is_empty() {
                    input.push(json!({ "role": "user", "content": content }));
                }
            }
            Message::Assistant(a) => {
                for block in &a.content {
                    match block {
                        ContentBlock::Thinking(t) => {
                            if let Some(item) = reasoning_item(&t.signature) {
                                input.push(item);
                            }
                            // Invalid/missing opaque reasoning is not replayed as
                            // visible assistant text.
                        }
                        ContentBlock::Text(t) => {
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "status": "completed",
                                "content": [{ "type": "output_text", "text": t.text, "annotations": [] }],
                            });
                            if let Some(signature) = parse_text_signature(t.signature.as_deref()) {
                                item["id"] = json!(replay_text_id(&signature.id));
                                if let Some(phase) = signature.phase {
                                    item["phase"] = json!(phase);
                                }
                            } else {
                                item["id"] = json!(replay_text_id(&t.text));
                            }
                            input.push(item);
                        }
                        ContentBlock::ToolCall(c) => {
                            let (call_id, item_id) = split_call_id(&c.id);
                            let mut item = json!({
                                "type": "function_call",
                                "call_id": normalize_tool_call_id(call_id, 64),
                                "name": c.name,
                                "arguments": serde_json::to_string(&c.arguments).map_err(|_| {
                                    HarnessError::InvalidRequest("tool arguments could not be serialized".into())
                                })?,
                            });
                            if let Some(item_id) = item_id {
                                item["id"] = json!(normalize_tool_call_id(item_id, 64));
                            }
                            input.push(item);
                        }
                    }
                }
            }
            Message::ToolResult(r) => {
                let (call_id, _) = split_call_id(&r.tool_call_id);
                let output: Value = match r.content.as_slice() {
                    [ToolResultPart::Text(t)] => Value::String(t.clone()),
                    parts => Value::Array(
                        parts
                            .iter()
                            .map(|p| match p {
                                ToolResultPart::Text(t) => {
                                    json!({ "type": "input_text", "text": t })
                                }
                                ToolResultPart::Image {
                                    mime_type,
                                    data_b64,
                                } => json!({
                                    "type": "input_image",
                                    "detail": "auto",
                                    "image_url": format!("data:{mime_type};base64,{data_b64}"),
                                }),
                            })
                            .collect(),
                    ),
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": normalize_tool_call_id(call_id, 64),
                    "output": output,
                }));
            }
        }
    }

    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
        "store": false,
    });
    if let Some(max) = opts.max_tokens {
        body["max_output_tokens"] = json!(max.max(MIN_MAX_OUTPUT_TOKENS));
    }
    if !opts.omit_sampling
        && let Some(t) = opts.temperature
    {
        body["temperature"] = json!(t);
    }
    if let Some(effort) = &opts.reasoning {
        let mut reasoning = json!({ "effort": effort });
        if !opts.omit_reasoning_summary {
            reasoning["summary"] = json!("auto");
        }
        body["reasoning"] = reasoning;
    }
    if let Some(tier) = opts.service_tier {
        body["service_tier"] = json!(tier);
    }
    if !ctx.tools.is_empty() {
        body["tools"] = Value::Array(
            ctx.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                        "strict": false,
                    })
                })
                .collect(),
        );
    }
    Ok(body)
}

fn reasoning_item(signature: &Option<String>) -> Option<Value> {
    let signature = signature.as_deref()?;
    let item = serde_json::from_str::<Value>(signature).ok()?;
    item.is_object().then_some(item)
}

#[derive(Debug)]
struct TextSignature {
    id: String,
    phase: Option<String>,
}

fn parse_text_signature(signature: Option<&str>) -> Option<TextSignature> {
    let signature = signature?.trim();
    if signature.is_empty() {
        return None;
    }
    if signature.starts_with('{')
        && let Ok(value) = serde_json::from_str::<Value>(signature)
        && value["v"].as_u64() == Some(1)
        && let Some(id) = value["id"].as_str()
    {
        let phase = value["phase"].as_str().map(str::to_string);
        return Some(TextSignature {
            id: id.to_string(),
            phase,
        });
    }
    Some(TextSignature {
        id: signature.to_string(),
        phase: None,
    })
}

fn replay_text_id(id: &str) -> String {
    let raw = if id.is_empty() { "msg" } else { id };
    if raw.len() <= 64 {
        let normalized = normalize_tool_call_id(raw, 64);
        if normalized.is_empty() {
            format!("msg_{}", short_hash(raw))
        } else {
            normalized
        }
    } else {
        format!("msg_{}", short_hash(raw))
    }
}

/// One slot per streaming output index, with identifier aliases for lossy or
/// identifier-deviant Responses proxies.
#[derive(Default)]
pub struct Slots {
    by_wire: Vec<(i64, usize)>,
    by_alias: Vec<(String, usize)>,
}

impl Slots {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, wire: i64) -> Option<usize> {
        self.by_wire
            .iter()
            .find(|(w, _)| *w == wire)
            .map(|(_, i)| *i)
    }

    fn get_alias(&self, alias: &str) -> Option<usize> {
        self.by_alias
            .iter()
            .find(|(candidate, _)| candidate == alias)
            .map(|(_, i)| *i)
    }

    fn lookup(&self, value: &Value) -> Option<usize> {
        if let Some(wire) = value["output_index"].as_i64()
            && let Some(index) = self.get(wire)
        {
            return Some(index);
        }
        for key in ["item_id", "call_id", "id"] {
            if let Some(alias) = value[key].as_str() {
                if let Some(index) = self.get_alias(alias) {
                    return Some(index);
                }
                if key == "item_id"
                    && let Some(index) = self.get_alias(alias.strip_prefix("fc_").unwrap_or(alias))
                {
                    return Some(index);
                }
                if key == "call_id"
                    && let Some(index) = self.get_alias(&format!("fc_{alias}"))
                {
                    return Some(index);
                }
            }
        }
        None
    }

    fn insert(
        &mut self,
        wire: Option<i64>,
        content: usize,
        aliases: impl IntoIterator<Item = String>,
    ) {
        if let Some(wire) = wire {
            if let Some(existing) = self.by_wire.iter_mut().find(|(w, _)| *w == wire) {
                existing.1 = content;
            } else {
                self.by_wire.push((wire, content));
            }
        }
        for alias in aliases {
            if alias.is_empty() {
                continue;
            }
            if let Some(existing) = self
                .by_alias
                .iter_mut()
                .find(|(candidate, _)| candidate == &alias)
            {
                existing.1 = content;
            } else {
                self.by_alias.push((alias, content));
            }
        }
    }
}

fn apply_call_ids(scratch: &mut AssistantScratch, ci: usize, v: &Value) {
    let call_id = v["call_id"].as_str();
    let item_id = v["item_id"].as_str().or_else(|| v["item"]["id"].as_str());
    if let (Some(call_id), Some(item_id)) = (call_id, item_id) {
        scratch.set_tool_call_composite_id(ci, &composite_id(Some(call_id), Some(item_id)));
    } else if let Some(call_id) = call_id {
        scratch.set_tool_call_id(ci, call_id);
    }
    if let Some(name) = v["name"].as_str().or_else(|| v["item"]["name"].as_str()) {
        scratch.set_tool_call_name(ci, name);
    }
}

pub fn process_payload(
    payload: &str,
    scratch: &mut AssistantScratch,
    slots: &mut Slots,
) -> Result<PayloadOutcome, HarnessError> {
    let v: Value = serde_json::from_str(payload).map_err(|e| HarnessError::Malformed {
        format: FORMAT,
        detail: format!("bad JSON event: {e}"),
    })?;
    let kind = v["type"].as_str().unwrap_or_default();
    let mut events: Vec<StreamEvent> = Vec::new();
    match kind {
        "response.output_item.added" => {
            let wire = v["output_index"].as_i64();
            match v["item"]["type"].as_str().unwrap_or_default() {
                "message" => {
                    let (ci, ev) = scratch.open_text();
                    let id = v["item"]["id"].as_str().unwrap_or_default();
                    slots.insert(wire, ci, [id.to_string()]);
                    events.push(ev);
                }
                "reasoning" => {
                    let (ci, ev) = scratch.open_thinking(None, false);
                    let id = v["item"]["id"].as_str().unwrap_or_default();
                    slots.insert(wire, ci, [id.to_string()]);
                    events.push(ev);
                }
                "function_call" => {
                    let call_id = v["item"]["call_id"].as_str().unwrap_or_default();
                    let item_id = v["item"]["id"].as_str();
                    let id = composite_id(Some(call_id), item_id);
                    let name = v["item"]["name"].as_str().unwrap_or_default();
                    let (ci, ev) = scratch.open_tool_call(&id, name);
                    let aliases = [
                        call_id.to_string(),
                        item_id.unwrap_or_default().to_string(),
                        format!("fc_{call_id}"),
                    ];
                    slots.insert(wire, ci, aliases);
                    events.push(ev);
                    if let Some(arguments) = v["item"]["arguments"].as_str()
                        && !arguments.is_empty()
                    {
                        events.push(scratch.tool_call_delta(ci, arguments));
                    }
                }
                _ => {}
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(ci) = slots.lookup(&v)
                && let Some(d) = v["delta"].as_str()
            {
                events.push(scratch.thinking_delta(ci, d));
            }
        }
        "response.output_text.delta" | "response.refusal.delta" => {
            if let Some(ci) = slots.lookup(&v)
                && let Some(d) = v["delta"].as_str()
            {
                events.push(scratch.text_delta(ci, d));
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(ci) = slots.lookup(&v) {
                apply_call_ids(scratch, ci, &v);
                if let Some(fragment) = v["delta"].as_str().filter(|f| !f.is_empty()) {
                    events.push(scratch.tool_call_delta(ci, fragment));
                }
            }
        }
        "response.function_call_arguments.done" => {
            if let Some(ci) = slots.lookup(&v) {
                apply_call_ids(scratch, ci, &v);
                if let Some(arguments) = v["arguments"].as_str().filter(|f| !f.is_empty()) {
                    events.push(scratch.replace_tool_call_args(ci, arguments));
                }
            }
        }
        "response.output_item.done" => {
            if slots.lookup(&v).is_none() && v["item"]["type"].as_str() == Some("message") {
                let (ci, ev) = scratch.open_text();
                let id = v["item"]["id"].as_str().unwrap_or_default();
                slots.insert(v["output_index"].as_i64(), ci, [id.to_string()]);
                events.push(ev);
            }
            if let Some(ci) = slots.lookup(&v) {
                match v["item"]["type"].as_str().unwrap_or_default() {
                    "reasoning" => {
                        scratch.set_thinking_signature(ci, &v["item"].to_string());
                        events.push(scratch.thinking_end(ci));
                    }
                    "message" => {
                        let item = &v["item"];
                        if let Some(text) = message_item_text(item) {
                            scratch.replace_text(ci, &text);
                        }
                        let id = item["id"].as_str().unwrap_or_default();
                        let phase = item["phase"].as_str();
                        scratch.set_text_signature(ci, &encode_text_signature(id, phase));
                        events.push(scratch.text_end(ci));
                    }
                    "function_call" => {
                        let call_id = v["item"]["call_id"].as_str();
                        let item_id = v["item"]["id"].as_str();
                        if call_id.is_some() || item_id.is_some() {
                            scratch.set_tool_call_composite_id(ci, &composite_id(call_id, item_id));
                        }
                        if let Some(name) = v["item"]["name"].as_str() {
                            scratch.set_tool_call_name(ci, name);
                        }
                        if let Some(arguments) = v["item"]["arguments"].as_str()
                            && !arguments.is_empty()
                        {
                            events.push(scratch.replace_tool_call_args(ci, arguments));
                        }
                        events.push(scratch.end_tool_call(ci)?);
                    }
                    _ => {}
                }
            }
        }
        "response.completed" | "response.incomplete" => {
            let response = &v["response"];
            if let Some(id) = response["id"].as_str() {
                scratch.set_response_id(id);
            }
            apply_usage(scratch, &response["usage"]);
            let (reason, err) = map_stop_reason(response);
            if reason == StopReason::Stop && scratch.msg.tool_calls().next().is_some() {
                let (msg, ev) = scratch.finish_in_place(StopReason::ToolUse, err);
                events.push(ev);
                return Ok(PayloadOutcome::Terminal(msg, events));
            }
            let (msg, ev) = scratch.finish_in_place(reason, err);
            events.push(ev);
            return Ok(PayloadOutcome::Terminal(msg, events));
        }
        "response.failed" | "error" => {
            let detail = extract_error(&v);
            let (msg, ev) = scratch.fail_in_place(HarnessError::InvalidTerminal {
                format: FORMAT,
                detail,
            });
            events.push(ev);
            return Ok(PayloadOutcome::Terminal(msg, events));
        }
        _ => {}
    }
    Ok(PayloadOutcome::Events(events))
}

fn message_item_text(item: &Value) -> Option<String> {
    let parts = item.get("content")?.as_array()?;
    let mut text = String::new();
    let mut found = false;
    for part in parts {
        let piece = match part.get("type").and_then(Value::as_str) {
            Some("output_text") | Some("text") => part.get("text").and_then(Value::as_str),
            Some("refusal") => part.get("refusal").and_then(Value::as_str),
            _ => None,
        };
        if let Some(piece) = piece {
            text.push_str(piece);
            found = true;
        }
    }
    found.then_some(text)
}

fn encode_text_signature(id: &str, phase: Option<&str>) -> String {
    let mut value = json!({ "v": 1, "id": id });
    if let Some(phase @ ("commentary" | "final_answer")) = phase {
        value["phase"] = json!(phase);
    }
    value.to_string()
}

fn apply_usage(scratch: &mut AssistantScratch, usage: &Value) {
    let Some(u) = usage.as_object() else { return };
    let cached = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if let Some(input) = u.get("input_tokens").and_then(Value::as_u64) {
        scratch.usage_mut().input = input.saturating_sub(cached + cache_write);
    }
    if let Some(output) = u.get("output_tokens").and_then(Value::as_u64) {
        scratch.usage_mut().output = output;
    }
    scratch.usage_mut().cache_read = cached;
    scratch.usage_mut().cache_write = cache_write;
    if let Some(reasoning) = u
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
    {
        scratch.usage_mut().reasoning = Some(reasoning);
    }
    let current = *scratch.usage_mut();
    scratch.usage_mut().total_tokens = u
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            crate::model::Usage::derived_total(
                current.input,
                current.output,
                current.cache_read,
                current.cache_write,
            )
        });
}

fn map_stop_reason(response: &Value) -> (StopReason, Option<String>) {
    let status = response["status"].as_str().unwrap_or("completed");
    match status {
        "completed" => (StopReason::Stop, None),
        "incomplete" => {
            let reason = response["incomplete_details"]["reason"]
                .as_str()
                .unwrap_or_default();
            if reason == "max_output_tokens" {
                (StopReason::Length, None)
            } else {
                (
                    StopReason::Error,
                    Some(format!("response incomplete: {reason}")),
                )
            }
        }
        "failed" | "cancelled" => (
            StopReason::Error,
            Some(format!("provider status: {status}")),
        ),
        "in_progress" | "queued" => (
            StopReason::Error,
            Some(format!("non-terminal response status: {status}")),
        ),
        other => (
            StopReason::Error,
            Some(format!("unhandled response status: {other}")),
        ),
    }
}

fn extract_error(v: &Value) -> String {
    if let Some(err) = v.get("error") {
        let code = err["code"].as_str().unwrap_or("unknown");
        let msg = err["message"].as_str().unwrap_or("no message");
        return format!("{code}: {msg}");
    }
    if let Some(code) = v["code"].as_i64() {
        let msg = v["message"].as_str().unwrap_or("no message");
        return format!("error code {code}: {msg}");
    }
    "unknown error (no details in response)".to_string()
}

fn split_call_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once('|') {
        Some((call, item)) => (call, Some(item)),
        None => (id, None),
    }
}

fn composite_id(call_id: Option<&str>, item_id: Option<&str>) -> String {
    match (call_id, item_id) {
        (Some(c), Some(i)) if !i.is_empty() => format!("{c}|{i}"),
        (Some(c), _) => c.to_string(),
        (None, Some(i)) => i.to_string(),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AssistantScratch;
    use crate::model::ContentBlock;

    fn scratch() -> AssistantScratch {
        AssistantScratch::new("m", "p")
    }

    #[test]
    fn function_call_done_replaces_rather_than_appends_arguments() {
        let mut scratch = scratch();
        let mut slots = Slots::new();
        let added = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "function_call", "call_id": "c1", "id": "fc1", "name": "read" }
        });
        process_payload(&added.to_string(), &mut scratch, &mut slots).unwrap();
        let delta = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"path\":"
        });
        process_payload(&delta.to_string(), &mut scratch, &mut slots).unwrap();
        let done = json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "arguments": "{\"path\":\"src/lib.rs\"}"
        });
        process_payload(&done.to_string(), &mut scratch, &mut slots).unwrap();
        let finalize = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "function_call", "call_id": "c1", "id": "fc1", "name": "read", "arguments": "{\"path\":\"src/lib.rs\"}" }
        });
        process_payload(&finalize.to_string(), &mut scratch, &mut slots).unwrap();
        let ContentBlock::ToolCall(call) = &scratch.msg.content[0] else {
            panic!()
        };
        assert_eq!(call.arguments["path"], "src/lib.rs");
    }

    #[test]
    fn function_call_done_only_finalizes_complete_arguments() {
        let mut scratch = scratch();
        let mut slots = Slots::new();
        let added = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "function_call", "call_id": "c2", "id": "fc2", "name": "read" }
        });
        process_payload(&added.to_string(), &mut scratch, &mut slots).unwrap();
        let finalize = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "function_call", "call_id": "c2", "id": "fc2", "name": "read", "arguments": "{\"path\":\"only.rs\"}" }
        });
        process_payload(&finalize.to_string(), &mut scratch, &mut slots).unwrap();
        let ContentBlock::ToolCall(call) = &scratch.msg.content[0] else {
            panic!()
        };
        assert_eq!(call.arguments["path"], "only.rs");
    }

    #[test]
    fn message_done_does_not_replay_full_text_after_deltas() {
        let mut scratch = scratch();
        let mut slots = Slots::new();
        let added = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "message", "id": "msg1" }
        });
        process_payload(&added.to_string(), &mut scratch, &mut slots).unwrap();
        let delta = json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hello"
        });
        process_payload(&delta.to_string(), &mut scratch, &mut slots).unwrap();
        let done = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "message", "id": "msg1", "content": [{"type":"output_text","text":"hello world"}] }
        });
        process_payload(&done.to_string(), &mut scratch, &mut slots).unwrap();
        let ContentBlock::Text(text) = &scratch.msg.content[0] else {
            panic!()
        };
        assert_eq!(text.text, "hello world");
    }

    #[test]
    fn message_done_only_stores_full_text() {
        let mut scratch = scratch();
        let mut slots = Slots::new();
        let done = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "id": "msg2",
                "content": [{"type":"output_text","text":"only final"}]
            }
        });
        process_payload(&done.to_string(), &mut scratch, &mut slots).unwrap();
        let ContentBlock::Text(text) = &scratch.msg.content[0] else {
            panic!()
        };
        assert_eq!(text.text, "only final");
    }

    #[test]
    fn omit_sampling_drops_temperature_from_codex_body() {
        let body = build_body(
            &crate::model::PromptContext::default(),
            "gpt-5",
            &crate::model::RequestOptions {
                temperature: Some(0.7),
                omit_sampling: true,
                reasoning: Some("low".into()),
                omit_reasoning_summary: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(body.get("temperature").is_none());
        assert_eq!(body["reasoning"]["effort"], "low");
        assert!(body["reasoning"].get("summary").is_none());
    }
}
