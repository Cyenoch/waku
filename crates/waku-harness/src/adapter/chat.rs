//! OpenAI-compatible Chat Completions wire adapter (`/v1/chat/completions`).

use super::{AssistantScratch, PayloadOutcome};
use crate::error::HarnessError;
use crate::events::StreamEvent;
use crate::model::{
    ContentBlock, Message, PromptContext, RequestOptions, StopReason, ToolResultPart, UserPart,
};
use crate::transform::{normalize_tool_call_id, repaired_messages};
use serde_json::{Value, json};

pub const FORMAT: &str = "openai-chat-completions";

pub fn build_body(
    ctx: &PromptContext,
    model: &str,
    opts: &RequestOptions,
) -> Result<Value, HarnessError> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &ctx.system_prompt {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for msg in repaired_messages(&ctx.messages).iter() {
        match msg {
            Message::User(u) => {
                if u.parts.iter().all(|p| matches!(p, UserPart::Text(_))) {
                    let text = user_message_text(u);
                    messages.push(json!({ "role": "user", "content": text }));
                } else {
                    let content: Vec<Value> = u
                        .parts
                        .iter()
                        .map(|p| match p {
                            UserPart::Text(t) => json!({ "type": "text", "text": t }),
                            UserPart::Image { mime_type, data_b64 } => json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{mime_type};base64,{data_b64}") },
                            }),
                        })
                        .collect();
                    messages.push(json!({ "role": "user", "content": content }));
                }
            }
            Message::Assistant(a) => {
                let text: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let tool_calls: Result<Vec<Value>, HarnessError> = a
                    .tool_calls()
                    .map(|c| {
                        Ok(json!({
                            "id": normalize_tool_call_id(&c.id, 40),
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": serde_json::to_string(&c.arguments).map_err(|_| {
                                    HarnessError::InvalidRequest("tool arguments could not be serialized".into())
                                })?,
                            }
                        }))
                    })
                    .collect();
                let tool_calls = tool_calls?;
                if text.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut m = json!({ "role": "assistant", "content": text });
                if !tool_calls.is_empty() {
                    m["tool_calls"] = Value::Array(tool_calls);
                }
                // Replay reasoning under its recorded dialect field.
                if let Some(ContentBlock::Thinking(t)) = a
                    .content
                    .iter()
                    .find(|b| matches!(b, ContentBlock::Thinking(_)))
                    && !t.thinking.is_empty()
                {
                    let field = t
                        .signature
                        .clone()
                        .unwrap_or_else(|| "reasoning_content".into());
                    m[field] = Value::String(t.thinking.clone());
                }
                messages.push(m);
            }
            Message::ToolResult(r) => {
                let text = r
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ToolResultPart::Text(t) => Some(t.as_str()),
                        ToolResultPart::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = r
                    .content
                    .iter()
                    .any(|p| matches!(p, ToolResultPart::Image { .. }));
                let body = if text.is_empty() && has_images {
                    "(see attached image)".to_string()
                } else if text.is_empty() {
                    "(no tool output)".to_string()
                } else {
                    text
                };
                let call_id = r
                    .tool_call_id
                    .split_once('|')
                    .map_or(r.tool_call_id.as_str(), |(call, _)| call);
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": normalize_tool_call_id(call_id, 40),
                    "content": body,
                }));
                // Tool-result images ride a following user message.
                let images: Vec<Value> = r
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ToolResultPart::Image {
                            mime_type,
                            data_b64,
                        } => Some(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{mime_type};base64,{data_b64}") },
                        })),
                        _ => None,
                    })
                    .collect();
                if !images.is_empty() {
                    let mut parts = vec![
                        json!({ "type": "text", "text": "Attached image(s) from tool result:" }),
                    ];
                    parts.extend(images);
                    messages.push(json!({ "role": "user", "content": parts }));
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(max) = opts.max_tokens {
        body["max_tokens"] = json!(max);
    }
    if !opts.omit_sampling
        && let Some(t) = opts.temperature
    {
        body["temperature"] = json!(t);
    }
    if let Some(effort) = &opts.reasoning {
        body["reasoning_effort"] = json!(effort);
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
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect(),
        );
    }
    Ok(body)
}

#[derive(Default)]
pub struct ChatState {
    /// delta tool index → content index
    by_delta: Vec<(i64, usize)>,
    text_index: Option<usize>,
    reasoning_index: Option<usize>,
    finished: Option<String>,
    finish_error: Option<String>,
}

impl ChatState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub fn process_payload(
    payload: &str,
    scratch: &mut AssistantScratch,
    state: &mut ChatState,
) -> Result<PayloadOutcome, HarnessError> {
    let v: Value = serde_json::from_str(payload).map_err(|e| HarnessError::Malformed {
        format: FORMAT,
        detail: format!("bad JSON chunk: {e}"),
    })?;
    let mut events: Vec<StreamEvent> = Vec::new();

    if let Some(id) = v["id"].as_str() {
        scratch.set_response_id(id);
    }
    let has_usage = v["usage"].is_object();
    if let Some(u) = v["usage"].as_object() {
        apply_usage(scratch, u);
    }

    let choice = match v["choices"].get(0) {
        Some(c) => c,
        None => {
            if state.finished.is_some() && has_usage {
                return finish_pending(scratch, state, events);
            }
            return Ok(PayloadOutcome::Events(events));
        }
    };
    if let Some(reason) = choice["finish_reason"].as_str() {
        state.finished = Some(reason.to_string());
        let (stop, err) = map_finish_reason(reason);
        scratch.msg.stop_reason = stop;
        state.finish_error = err.clone();
        scratch.msg.error_message = err;
    }

    let delta = &choice["delta"];
    // Reasoning fields vary by dialect; take the first non-empty.
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
        if let Some(d) = delta[field].as_str()
            && !d.is_empty()
        {
            match state.reasoning_index {
                Some(ci) => events.push(scratch.thinking_delta(ci, d)),
                None => {
                    let (ci, ev) = scratch.open_thinking(Some(field.to_string()), false);
                    state.reasoning_index = Some(ci);
                    events.push(ev);
                    events.push(scratch.thinking_delta(ci, d));
                }
            }
            break;
        }
    }
    if let Some(d) = delta["content"].as_str()
        && !d.is_empty()
    {
        match state.text_index {
            Some(ci) => events.push(scratch.text_delta(ci, d)),
            None => {
                let (ci, ev) = scratch.open_text();
                state.text_index = Some(ci);
                events.push(ev);
                events.push(scratch.text_delta(ci, d));
            }
        }
    }
    if let Some(tool_deltas) = delta["tool_calls"].as_array() {
        for td in tool_deltas {
            let di = td["index"].as_i64().unwrap_or(0);
            let existing = state
                .by_delta
                .iter()
                .find(|(d, _)| *d == di)
                .map(|(_, c)| *c);
            let ci = match existing {
                Some(c) => c,
                None => {
                    let call_id = td["id"].as_str().unwrap_or_default().to_string();
                    let name = td["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let (c, ev) = scratch.open_tool_call(&call_id, &name);
                    state.by_delta.push((di, c));
                    events.push(ev);
                    c
                }
            };
            // Some servers send id/name on a later fragment than the open.
            if let Some(id) = td["id"].as_str()
                && !id.is_empty()
            {
                scratch.set_tool_call_id(ci, id);
            }
            if let Some(name) = td["function"]["name"].as_str()
                && !name.is_empty()
            {
                scratch.set_tool_call_name(ci, name);
            }
            if let Some(args) = td["function"]["arguments"].as_str()
                && !args.is_empty()
            {
                events.push(scratch.tool_call_delta(ci, args));
            }
        }
    }

    // Do not finalize on finish_reason alone. Providers commonly send an
    // empty choices usage chunk or [DONE] after the final tool fragment.
    if state.finished.is_some() && has_usage {
        return finish_pending(scratch, state, events);
    }
    Ok(PayloadOutcome::Events(events))
}

/// Finalize a Chat stream at `[DONE]` or EOF after a finish reason was seen.
pub fn finish_pending(
    scratch: &mut AssistantScratch,
    state: &mut ChatState,
    mut events: Vec<StreamEvent>,
) -> Result<PayloadOutcome, HarnessError> {
    let Some(reason) = state.finished.as_deref() else {
        return Ok(PayloadOutcome::Events(events));
    };
    let (stop, mapped_error) = map_finish_reason(reason);
    close_open_blocks(scratch, state, &mut events)?;
    let error = state.finish_error.take().or(mapped_error);
    let (msg, ev) = match stop {
        StopReason::Error => scratch.fail_in_place(HarnessError::InvalidTerminal {
            format: FORMAT,
            detail: error
                .clone()
                .unwrap_or_else(|| "provider stream failed".into()),
        }),
        _ => scratch.finish_in_place(stop, error),
    };
    events.push(ev);
    Ok(PayloadOutcome::Terminal(msg, events))
}

fn close_open_blocks(
    scratch: &mut AssistantScratch,
    state: &mut ChatState,
    events: &mut Vec<StreamEvent>,
) -> Result<(), HarnessError> {
    if let Some(ci) = state.text_index.take() {
        events.push(scratch.text_end(ci));
    }
    if let Some(ci) = state.reasoning_index.take() {
        events.push(scratch.thinking_end(ci));
    }
    let calls = std::mem::take(&mut state.by_delta);
    for (_, ci) in calls {
        events.push(scratch.end_tool_call(ci)?);
    }
    Ok(())
}

fn map_finish_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" => (
            StopReason::Error,
            Some("provider finish_reason: content_filter".into()),
        ),
        "network_error" => (
            StopReason::Error,
            Some("provider finish_reason: network_error".into()),
        ),
        other => (
            StopReason::Error,
            Some(format!("provider finish_reason: {other}")),
        ),
    }
}

fn apply_usage(scratch: &mut AssistantScratch, u: &serde_json::Map<String, Value>) {
    let prompt = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| u.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = u
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = u
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    scratch.usage_mut().input = prompt.saturating_sub(cached + cache_write);
    scratch.usage_mut().cache_read = cached;
    scratch.usage_mut().cache_write = cache_write;
    scratch.usage_mut().output = output;
    scratch.usage_mut().reasoning = reasoning;
    scratch.usage_mut().total_tokens = u
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            crate::model::Usage::derived_total(
                prompt.saturating_sub(cached + cache_write),
                output,
                cached,
                cache_write,
            )
        });
}

fn user_message_text(u: &crate::model::UserMessage) -> String {
    crate::model::UserMessage::text_of(&u.parts)
}
