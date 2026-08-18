//! Anthropic Messages wire adapter (`/v1/messages`).

use super::{AssistantScratch, PayloadOutcome};
use crate::error::HarnessError;
use crate::events::StreamEvent;
use crate::model::{
    AssistantMessage, ContentBlock, Message, PromptContext, RequestOptions, StopReason,
    ToolResultPart, UserPart,
};
use crate::transform::{normalize_tool_call_id, repaired_messages};
use serde_json::{Value, json};

pub const FORMAT: &str = "anthropic-messages";

pub fn build_body(
    ctx: &PromptContext,
    model: &str,
    opts: &RequestOptions,
) -> Result<Value, HarnessError> {
    if opts.service_tier.is_some() {
        return Err(HarnessError::InvalidRequest(
            "service tier is not supported by Anthropic".into(),
        ));
    }
    let mut messages: Vec<Value> = Vec::new();
    for msg in repaired_messages(&ctx.messages).iter() {
        match msg {
            Message::User(u) => {
                let blocks: Vec<Value> = u
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        UserPart::Text(t) if !t.trim().is_empty() => {
                            Some(json!({ "type": "text", "text": t }))
                        }
                        UserPart::Image { mime_type, data_b64 } => Some(json!({
                            "type": "image",
                            "source": { "type": "base64", "media_type": mime_type, "data": data_b64 }
                        })),
                        _ => None,
                    })
                    .collect();
                if !blocks.is_empty() {
                    messages.push(json!({ "role": "user", "content": blocks }));
                }
            }
            Message::Assistant(a) => {
                let blocks: Vec<Value> = assistant_blocks(a);
                if !blocks.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Message::ToolResult(r) => {
                let content: Vec<Value> = r
                    .content
                    .iter()
                    .map(|p| match p {
                        ToolResultPart::Text(t) => json!({ "type": "text", "text": t }),
                        ToolResultPart::Image { mime_type, data_b64 } => json!({
                            "type": "image",
                            "source": { "type": "base64", "media_type": mime_type, "data": data_b64 }
                        }),
                    })
                    .collect();
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": normalize_tool_call_id(&r.tool_call_id, 64),
                    "content": content,
                    "is_error": r.is_error,
                });
                let append_to_last_user = messages
                    .last()
                    .is_some_and(|last| last["role"] == "user" && last["content"].is_array());
                if append_to_last_user {
                    if let Some(content) = messages
                        .last_mut()
                        .and_then(|last| last["content"].as_array_mut())
                    {
                        content.push(block);
                    }
                } else {
                    messages.push(json!({ "role": "user", "content": [block] }));
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": opts.max_tokens.unwrap_or(8192),
        "stream": true,
    });
    if let Some(system) = &ctx.system_prompt {
        body["system"] = json!([{ "type": "text", "text": system }]);
    }
    if opts.reasoning.is_none()
        && let Some(t) = opts.temperature
    {
        body["temperature"] = json!(t);
    }
    match &opts.reasoning {
        Some(effort) => {
            body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            body["output_config"] = json!({ "effort": effort });
        }
        None => {
            body["thinking"] = json!({ "type": "disabled" });
        }
    }
    if !ctx.tools.is_empty() {
        body["tools"] = Value::Array(
            ctx.tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect(),
        );
    }
    Ok(body)
}

fn assistant_blocks(a: &AssistantMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    for b in &a.content {
        match b {
            ContentBlock::Text(t) => {
                if !t.text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": t.text }));
                }
            }
            ContentBlock::Thinking(t) => {
                if t.redacted {
                    if let Some(data) = &t.signature {
                        blocks.push(json!({ "type": "redacted_thinking", "data": data }));
                    }
                } else if !t.thinking.trim().is_empty() {
                    match &t.signature {
                        Some(sig) if !sig.trim().is_empty() => blocks.push(json!({
                            "type": "thinking",
                            "thinking": t.thinking,
                            "signature": sig,
                        })),
                        _ => blocks.push(json!({ "type": "text", "text": t.thinking })),
                    }
                }
            }
            ContentBlock::ToolCall(c) => blocks.push(json!({
                "type": "tool_use",
                "id": normalize_tool_call_id(&c.id, 64),
                "name": c.name,
                "input": c.arguments,
            })),
        }
    }
    blocks
}

#[derive(Default)]
pub struct AnthropicState {
    by_wire: Vec<(i64, usize)>,
    stop_reason: Option<String>,
}

impl AnthropicState {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, wire: i64) -> Option<usize> {
        self.by_wire
            .iter()
            .find(|(w, _)| *w == wire)
            .map(|(_, i)| *i)
    }

    fn insert(&mut self, wire: i64, content: usize) {
        if let Some(existing) = self.by_wire.iter_mut().find(|(w, _)| *w == wire) {
            existing.1 = content;
        } else {
            self.by_wire.push((wire, content));
        }
    }

    fn remove(&mut self, wire: i64) -> Option<usize> {
        let position = self.by_wire.iter().position(|(w, _)| *w == wire)?;
        Some(self.by_wire.swap_remove(position).1)
    }
}

pub fn process_payload(
    payload: &str,
    scratch: &mut AssistantScratch,
    state: &mut AnthropicState,
) -> Result<PayloadOutcome, HarnessError> {
    let v: Value = serde_json::from_str(payload).map_err(|e| HarnessError::Malformed {
        format: FORMAT,
        detail: format!("bad JSON event: {e}"),
    })?;
    let kind = v["type"].as_str().unwrap_or_default();
    let mut events: Vec<StreamEvent> = Vec::new();
    match kind {
        "message_start" => {
            if let Some(id) = v["message"]["id"].as_str() {
                scratch.set_response_id(id);
            }
            apply_usage(scratch, &v["message"]["usage"]);
        }
        "content_block_start" => {
            let idx = v["index"].as_i64().unwrap_or(0);
            match v["content_block"]["type"].as_str().unwrap_or_default() {
                "text" => {
                    let (ci, ev) = scratch.open_text();
                    state.insert(idx, ci);
                    events.push(ev);
                    if let Some(text) = v["content_block"]["text"].as_str()
                        && !text.is_empty()
                    {
                        events.push(scratch.text_delta(ci, text));
                    }
                }
                "thinking" => {
                    let signature = v["content_block"]["signature"].as_str();
                    let (ci, ev) = scratch.open_thinking(
                        signature.filter(|sig| !sig.is_empty()).map(str::to_string),
                        false,
                    );
                    state.insert(idx, ci);
                    events.push(ev);
                    if let Some(thinking) = v["content_block"]["thinking"].as_str()
                        && !thinking.is_empty()
                    {
                        events.push(scratch.thinking_delta(ci, thinking));
                    }
                }
                "redacted_thinking" => {
                    let data = v["content_block"]["data"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let (ci, ev) = scratch.open_thinking(Some(data), true);
                    state.insert(idx, ci);
                    events.push(ev);
                }
                "tool_use" => {
                    let id = v["content_block"]["id"].as_str().unwrap_or_default();
                    let name = v["content_block"]["name"].as_str().unwrap_or_default();
                    let (ci, ev) = scratch.open_tool_call(id, name);
                    state.insert(idx, ci);
                    events.push(ev);
                    if !v["content_block"]["input"].is_null() {
                        let input = v["content_block"]["input"].to_string();
                        events.push(scratch.tool_call_delta(ci, &input));
                    }
                }
                _ => {}
            }
        }
        "content_block_delta" => {
            let idx = v["index"].as_i64().unwrap_or(0);
            if let Some(ci) = state.get(idx) {
                match v["delta"]["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        if let Some(d) = v["delta"]["text"].as_str() {
                            events.push(scratch.text_delta(ci, d));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(d) = v["delta"]["thinking"].as_str() {
                            events.push(scratch.thinking_delta(ci, d));
                        }
                    }
                    "signature_delta" => {
                        if let Some(d) = v["delta"]["signature"].as_str() {
                            scratch.append_thinking_signature(ci, d);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(d) = v["delta"]["partial_json"].as_str() {
                            events.push(scratch.tool_call_delta(ci, d));
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            let idx = v["index"].as_i64().unwrap_or(0);
            if let Some(ci) = state.remove(idx) {
                match scratch.msg.content.get(ci) {
                    Some(ContentBlock::Text(_)) => events.push(scratch.text_end(ci)),
                    Some(ContentBlock::Thinking(_)) => events.push(scratch.thinking_end(ci)),
                    Some(ContentBlock::ToolCall(_)) => events.push(scratch.end_tool_call(ci)?),
                    None => {}
                }
            }
        }
        "message_delta" => {
            if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                state.stop_reason = Some(reason.to_string());
            }
            if v["usage"].is_object() {
                apply_usage(scratch, &v["usage"]);
            }
        }
        "message_stop" => {
            close_open_blocks(scratch, state, &mut events)?;
            let reason = state
                .stop_reason
                .clone()
                .unwrap_or_else(|| "end_turn".into());
            let (stop, err) = map_stop_reason(&reason);
            match stop {
                StopReason::Error => {
                    let (msg, ev) = scratch.fail_in_place(HarnessError::InvalidTerminal {
                        format: FORMAT,
                        detail: err.unwrap_or_else(|| "provider stream failed".into()),
                    });
                    events.push(ev);
                    return Ok(PayloadOutcome::Terminal(msg, events));
                }
                _ => {
                    let (msg, ev) = scratch.finish_in_place(stop, err);
                    events.push(ev);
                    return Ok(PayloadOutcome::Terminal(msg, events));
                }
            }
        }
        "error" => {
            let detail = v["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .unwrap_or("unknown error")
                .to_string();
            let (msg, ev) = scratch.fail_in_place(HarnessError::InvalidTerminal {
                format: FORMAT,
                detail,
            });
            events.push(ev);
            return Ok(PayloadOutcome::Terminal(msg, events));
        }
        "ping" | "message_role" => {}
        _ => {}
    }
    Ok(PayloadOutcome::Events(events))
}

fn close_open_blocks(
    scratch: &mut AssistantScratch,
    state: &mut AnthropicState,
    events: &mut Vec<StreamEvent>,
) -> Result<(), HarnessError> {
    let open = std::mem::take(&mut state.by_wire);
    for (_, ci) in open {
        match scratch.msg.content.get(ci) {
            Some(ContentBlock::Text(_)) => events.push(scratch.text_end(ci)),
            Some(ContentBlock::Thinking(_)) => events.push(scratch.thinking_end(ci)),
            Some(ContentBlock::ToolCall(_)) => events.push(scratch.end_tool_call(ci)?),
            None => {}
        }
    }
    Ok(())
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" | "model_context_window_exceeded" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => (
            StopReason::Error,
            Some("the model refused to complete the request".into()),
        ),
        "sensitive" => (
            StopReason::Error,
            Some("provider stopped with: sensitive".into()),
        ),
        other => (
            StopReason::Error,
            Some(format!("unhandled stop reason: {other}")),
        ),
    }
}

fn apply_usage(scratch: &mut AssistantScratch, usage: &Value) {
    let Some(u) = usage.as_object() else { return };
    if let Some(i) = u.get("input_tokens").and_then(Value::as_u64) {
        scratch.usage_mut().input = i;
    }
    if let Some(o) = u.get("output_tokens").and_then(Value::as_u64) {
        scratch.usage_mut().output = o;
    }
    if let Some(c) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
        scratch.usage_mut().cache_read = c;
    }
    if let Some(c) = u.get("cache_creation_input_tokens").and_then(Value::as_u64) {
        scratch.usage_mut().cache_write = c;
    }
    let reasoning = u
        .get("output_tokens_details")
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(Value::as_u64);
    if let Some(r) = reasoning {
        scratch.usage_mut().reasoning = Some(r);
    }
    let usage = *scratch.usage_mut();
    scratch.usage_mut().total_tokens = u
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            crate::model::Usage::derived_total(
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
            )
        });
}
