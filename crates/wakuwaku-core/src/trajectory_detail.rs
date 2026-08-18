//! Provider-neutral detail projection for trajectory inspector tabs.
//!
//! Source is an explicit whitelist. Thought signatures, secret headers, host
//! paths, and image/base64 blobs never leave the daemon.

use serde_json::{Map, Value, json};

use wakuwaku_protocol::{
    MAX_WIRE_MESSAGE_BYTES, TrajectoryDetailContent, TrajectoryDetailSection, TrajectoryResponse,
    clamp_trajectory_detail_window,
};

use crate::trajectory::{
    TrajectoryKind, TrajectoryPrompt, TrajectoryRecord, TrajectorySessionMeta,
};

const OMITTED_BINARY: &str = "[omitted binary]";
const OMITTED_PATH: &str = "[omitted path]";
const OMITTED_SECRET: &str = "[omitted secret]";

pub struct TrajectoryDetailContext {
    pub meta: TrajectorySessionMeta,
    pub record: TrajectoryRecord,
    pub prompt: Option<TrajectoryPrompt>,
    pub previous_system_prompt: Option<String>,
}

pub fn project_detail(
    context: &TrajectoryDetailContext,
    section: TrajectoryDetailSection,
    cursor: Option<u64>,
    limit: Option<u32>,
) -> TrajectoryResponse {
    let window = clamp_trajectory_detail_window(limit) as usize;
    let offset = cursor.unwrap_or(0);
    let value = match section {
        TrajectoryDetailSection::Summary => summary_json(context),
        TrajectoryDetailSection::Preview => Value::String(context.record.preview.clone()),
        TrajectoryDetailSection::Raw => sanitized_detail_json(&context.record.detail_json),
        TrajectoryDetailSection::Source => source_json(context),
        TrajectoryDetailSection::SystemPrompt => prompt_text(context, |prompt| {
            prompt.system_prompt.clone().unwrap_or_default()
        }),
        TrajectoryDetailSection::Tools => {
            prompt_json(context, |prompt| parse_json(&prompt.tools_json))
        }
        TrajectoryDetailSection::Diff => diff_json(context),
        TrajectoryDetailSection::Options => {
            prompt_json(context, |prompt| parse_json(&prompt.options_json))
        }
        TrajectoryDetailSection::Usage => field_or_null(parsed_detail(&context.record), "usage"),
        TrajectoryDetailSection::Timing => timing_json(&context.record),
        TrajectoryDetailSection::Payload => payload_json(context),
        TrajectoryDetailSection::Result => result_json(context),
        TrajectoryDetailSection::Schema => schema_json(context),
    };
    let (content, next_cursor, has_more) = window_value(value, offset, window);
    TrajectoryResponse::Detail {
        record_id: context.record.record_id,
        section,
        generation: context.meta.generation.max(0) as u64,
        revision: context.meta.revision.max(0) as u64,
        content,
        next_cursor,
        has_more,
    }
}

pub fn utf8_byte_window(input: &str, offset: u64, limit: usize) -> (String, u64, u64, bool) {
    let bytes = input.as_bytes();
    let total = bytes.len() as u64;
    let mut start = (offset as usize).min(bytes.len());
    while start > 0 && start < bytes.len() && !input.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + limit).min(bytes.len());
    while end > start && end < bytes.len() && !input.is_char_boundary(end) {
        end -= 1;
    }
    let text = input[start..end].to_owned();
    let next = end as u64;
    (text, start as u64, total, next < total)
}

pub fn sanitize_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut clean = Map::new();
            for (key, child) in map {
                if is_denied_key(&key) {
                    continue;
                }
                clean.insert(key, sanitize_json(child));
            }
            Value::Object(clean)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_json).collect()),
        Value::String(text) => Value::String(sanitize_string(&text)),
        other => other,
    }
}

fn window_value(
    value: Value,
    offset: u64,
    limit: usize,
) -> (TrajectoryDetailContent, Option<u64>, bool) {
    match value {
        Value::String(text) => window_text(text, offset, limit),
        other => {
            let encoded = serde_json::to_string(&other).unwrap_or_else(|_| "null".into());
            if encoded.len() <= limit && offset == 0 {
                (
                    TrajectoryDetailContent {
                        json: Some(other),
                        text: None,
                        offset: 0,
                        byte_length: encoded.len() as u64,
                        total_bytes: encoded.len() as u64,
                    },
                    None,
                    false,
                )
            } else {
                window_text(encoded, offset, limit)
            }
        }
    }
}

fn window_text(
    text: String,
    offset: u64,
    limit: usize,
) -> (TrajectoryDetailContent, Option<u64>, bool) {
    let (slice, start, total, has_more) = utf8_byte_window(&text, offset, limit);
    let next = if has_more {
        Some(start + slice.len() as u64)
    } else {
        None
    };
    (
        TrajectoryDetailContent {
            json: None,
            text: Some(slice.clone()),
            offset: start,
            byte_length: slice.len() as u64,
            total_bytes: total,
        },
        next,
        has_more,
    )
}

fn summary_json(context: &TrajectoryDetailContext) -> Value {
    json!({
        "recordId": context.record.record_id,
        "kind": context.record.kind.as_str(),
        "lane": context.record.lane.as_str(),
        "status": context.record.status.as_str(),
        "title": context.record.title,
        "turnCount": context.record.turn_count,
        "step": context.record.step,
        "preview": context.record.preview,
    })
}

fn source_json(context: &TrajectoryDetailContext) -> Value {
    let detail = parsed_detail(&context.record);
    let mut source = Map::new();
    source.insert("kind".into(), json!(context.record.kind.as_str()));
    source.insert("title".into(), json!(context.record.title));
    match context.record.kind {
        TrajectoryKind::System => {
            insert_string(&mut source, "modelHint", detail.get("model_hint"));
            if let Some(prompt) = &context.prompt {
                source.insert(
                    "systemPrompt".into(),
                    Value::String(sanitize_string(
                        prompt.system_prompt.as_deref().unwrap_or(""),
                    )),
                );
            }
        }
        TrajectoryKind::User | TrajectoryKind::Context => {
            insert_sanitized(&mut source, "text", detail.get("text"));
            insert_sanitized(&mut source, "displayText", detail.get("display_text"));
            insert_bool(&mut source, "hasImage", detail.get("has_image"));
            insert_bool(
                &mut source,
                "sourceMetadataMissing",
                detail.get("source_metadata_missing"),
            );
            if let Some(attachments) = detail.get("attachments") {
                source.insert("attachments".into(), sanitize_json(attachments.clone()));
            }
            insert_u64(&mut source, "steeringId", detail.get("steering_id"));
        }
        TrajectoryKind::Request => {
            insert_sanitized(&mut source, "model", detail.get("model"));
            insert_sanitized(&mut source, "provider", detail.get("provider"));
            insert_sanitized(&mut source, "error", detail.get("error"));
        }
        TrajectoryKind::Assistant => {
            insert_sanitized(&mut source, "model", detail.get("model"));
            insert_sanitized(&mut source, "provider", detail.get("provider"));
            insert_sanitized(&mut source, "stopReason", detail.get("stop_reason"));
            insert_sanitized(&mut source, "errorMessage", detail.get("error_message"));
            if let Some(usage) = detail.get("usage") {
                source.insert("usage".into(), sanitize_json(usage.clone()));
            }
            if let Some(blocks) = detail.get("blocks") {
                source.insert("blocks".into(), sanitize_json(blocks.clone()));
            }
        }
        TrajectoryKind::Tool => {
            insert_sanitized(&mut source, "callId", detail.get("call_id"));
            insert_sanitized(&mut source, "name", detail.get("name"));
            if let Some(arguments) = detail.get("arguments") {
                source.insert("arguments".into(), sanitize_json(arguments.clone()));
            }
            if let Some(result) = detail.get("result") {
                source.insert("result".into(), sanitize_json(result.clone()));
            }
        }
    }
    Value::Object(source)
}

fn timing_json(record: &TrajectoryRecord) -> Value {
    json!({
        "startedAtMs": record.started_at_ms,
        "firstTokenAtMs": record.first_token_at_ms,
        "completedAtMs": record.completed_at_ms,
        "durationMs": record.duration_ms,
        "ttftMs": record.ttft_ms,
    })
}

fn payload_json(context: &TrajectoryDetailContext) -> Value {
    let detail = parsed_detail(&context.record);
    detail
        .get("arguments")
        .cloned()
        .map(sanitize_json)
        .unwrap_or(Value::Null)
}

fn result_json(context: &TrajectoryDetailContext) -> Value {
    let detail = parsed_detail(&context.record);
    if let Some(result) = detail.get("result") {
        return sanitize_json(result.clone());
    }
    if let Some(preview) = detail.get("preview") {
        return sanitize_json(preview.clone());
    }
    Value::String(sanitize_string(&context.record.preview))
}

fn schema_json(context: &TrajectoryDetailContext) -> Value {
    let Some(prompt) = &context.prompt else {
        return Value::Null;
    };
    let tools = parse_json(&prompt.tools_json);
    let detail = parsed_detail(&context.record);
    let name = detail
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| detail.get("call_id").and_then(Value::as_str));
    match (tools, name) {
        (Value::Array(items), Some(name)) => items
            .into_iter()
            .find(|item| {
                item.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|candidate| candidate == name)
            })
            .map(sanitize_json)
            .unwrap_or(Value::Null),
        (tools, _) => sanitize_json(tools),
    }
}

fn diff_json(context: &TrajectoryDetailContext) -> Value {
    let current = context
        .prompt
        .as_ref()
        .and_then(|prompt| prompt.system_prompt.clone());
    json!({
        "previous": context.previous_system_prompt,
        "current": current,
        "changed": context.previous_system_prompt.as_deref() != current.as_deref()
            && context.previous_system_prompt.is_some(),
    })
}

fn prompt_text(
    context: &TrajectoryDetailContext,
    pick: impl Fn(&TrajectoryPrompt) -> String,
) -> Value {
    context
        .prompt
        .as_ref()
        .map(|prompt| Value::String(sanitize_string(&pick(prompt))))
        .unwrap_or(Value::Null)
}

fn prompt_json(
    context: &TrajectoryDetailContext,
    pick: impl Fn(&TrajectoryPrompt) -> Value,
) -> Value {
    context
        .prompt
        .as_ref()
        .map(|prompt| sanitize_json(pick(prompt)))
        .unwrap_or(Value::Null)
}

fn parsed_detail(record: &TrajectoryRecord) -> Value {
    sanitize_json(parse_json(&record.detail_json))
}

fn sanitized_detail_json(detail_json: &str) -> Value {
    sanitize_json(parse_json(detail_json))
}

fn parse_json(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

fn insert_sanitized(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value {
        map.insert(key.into(), sanitize_json(value.clone()));
    }
}

fn insert_string(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(text) = value.and_then(Value::as_str) {
        map.insert(key.into(), Value::String(sanitize_string(text)));
    }
}

fn insert_bool(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(flag) = value.and_then(Value::as_bool) {
        map.insert(key.into(), Value::Bool(flag));
    }
}

fn insert_u64(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(number) = value.and_then(Value::as_u64) {
        map.insert(key.into(), json!(number));
    }
}

fn field_or_null(value: Value, key: &str) -> Value {
    value
        .get(key)
        .cloned()
        .map(sanitize_json)
        .unwrap_or(Value::Null)
}

fn is_denied_key(key: &str) -> bool {
    matches!(
        normalize_key(key).as_str(),
        "signature"
            | "thoughtsignature"
            | "thought_signature"
            | "redactedsignature"
            | "authorization"
            | "cookie"
            | "setcookie"
            | "apikey"
            | "api_key"
            | "xapikey"
            | "x_api_key"
            | "secret"
            | "secretkey"
            | "privatekey"
            | "private_key"
            | "accesstoken"
            | "access_token"
            | "refreshtoken"
            | "refresh_token"
            | "password"
            | "hostpath"
            | "host_path"
            | "absolutepath"
            | "absolute_path"
    )
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|ch| *ch != '-' && *ch != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn sanitize_string(input: &str) -> String {
    if looks_like_secret_header(input) {
        return OMITTED_SECRET.into();
    }
    if looks_like_base64_blob(input) {
        return OMITTED_BINARY.into();
    }
    if looks_like_host_path(input) {
        return OMITTED_PATH.into();
    }
    input.to_owned()
}

fn looks_like_secret_header(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    lower.starts_with("bearer ") || lower.starts_with("basic ")
}

fn looks_like_base64_blob(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.starts_with("data:image") || trimmed.starts_with("data:application") {
        return true;
    }
    trimmed.len() > 256
        && trimmed.bytes().all(|byte| {
            matches!(
                byte,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'=' | b'\n' | b'\r'
            )
        })
}

fn looks_like_host_path(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.starts_with("file://") {
        return true;
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        return true;
    }
    trimmed.starts_with('/') && trimmed[1..].contains('/')
}

pub fn assert_detail_within_wire_limit(response: &TrajectoryResponse) {
    let encoded = serde_json::to_vec(response).expect("trajectory detail serializes");
    assert!(
        encoded.len() < MAX_WIRE_MESSAGE_BYTES,
        "trajectory detail response {} exceeded the 48 MiB wire cap",
        encoded.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trajectory::{TrajectoryAvailability, TrajectoryLane, TrajectoryStatus};
    use uuid::Uuid;
    use wakuwaku_protocol::MAX_WIRE_MESSAGE_BYTES;

    fn context_with_detail(detail: Value) -> TrajectoryDetailContext {
        TrajectoryDetailContext {
            meta: TrajectorySessionMeta {
                session_id: Uuid::from_u128(1),
                schema_version: 1,
                generation: 1,
                revision: 4,
                next_sequence: 2,
                availability: TrajectoryAvailability::Exact,
            },
            record: TrajectoryRecord {
                record_id: Uuid::from_u128(2),
                sequence: 1,
                revision: 4,
                request_id: None,
                parent_record_id: None,
                prompt_id: Some(Uuid::from_u128(3)),
                turn_count: 1,
                step: 0,
                kind: TrajectoryKind::Assistant,
                lane: TrajectoryLane::Model,
                status: TrajectoryStatus::Completed,
                title: "Assistant".into(),
                preview: "ok".into(),
                search_text: "ok".into(),
                started_at_ms: Some(1),
                first_token_at_ms: Some(2),
                completed_at_ms: Some(3),
                duration_ms: Some(2),
                ttft_ms: Some(1),
                detail_json: detail.to_string(),
            },
            prompt: Some(TrajectoryPrompt {
                prompt_id: Uuid::from_u128(3),
                sequence: 1,
                fingerprint: "fp".into(),
                system_prompt: Some("be careful".into()),
                tools_json: r#"[{"name":"read","input_schema":{"type":"object"}}]"#.into(),
                options_json: r#"{"temperature":0}"#.into(),
                model_hint: "model".into(),
                created_at_ms: 1,
            }),
            previous_system_prompt: Some("old".into()),
        }
    }

    #[test]
    fn source_whitelist_strips_signatures_secrets_paths_and_base64() {
        let context = context_with_detail(json!({
            "v": 1,
            "kind": "assistant",
            "model": "claude",
            "provider": "anthropic",
            "signature": "sig-think",
            "thought_signature": "sig-tool",
            "authorization": "Bearer secret-token",
            "host_path": "/Users/me/secret.rs",
            "blocks": [{
                "type": "thinking",
                "text": "plan",
                "signature": "keep-out",
                "thoughtSignature": "also-out"
            }, {
                "type": "text",
                "text": "data:image/png;base64,aaaa",
            }, {
                "type": "tool_call",
                "name": "read",
                "arguments": { "path": "/Users/me/notes.md" }
            }]
        }));
        let TrajectoryResponse::Detail { content, .. } =
            project_detail(&context, TrajectoryDetailSection::Source, None, None)
        else {
            panic!("expected detail");
        };
        let encoded = serde_json::to_string(&content.json).unwrap();
        assert!(!encoded.contains("sig-think"));
        assert!(!encoded.contains("sig-tool"));
        assert!(!encoded.contains("keep-out"));
        assert!(!encoded.contains("also-out"));
        assert!(!encoded.contains("Bearer secret-token"));
        assert!(!encoded.contains("/Users/me"));
        assert!(!encoded.contains("data:image/png;base64"));
        assert!(encoded.contains("plan"));
        assert!(encoded.contains(OMITTED_PATH));
        assert!(encoded.contains(OMITTED_BINARY));
    }

    #[test]
    fn utf8_windows_stay_on_character_boundaries() {
        let text = "é".repeat(8);
        let (slice, start, total, has_more) = utf8_byte_window(&text, 1, 3);
        assert_eq!(start, 0);
        assert!(slice.is_char_boundary(0));
        assert!(has_more);
        assert_eq!(total, text.len() as u64);
        assert!(!slice.contains('\u{FFFD}'));
    }

    #[test]
    fn huge_tool_result_stays_under_the_wire_cap() {
        let huge = (0..(MAX_WIRE_MESSAGE_BYTES / 32))
            .map(|index| format!("tool output line {index}\n"))
            .collect::<String>();
        let mut context = context_with_detail(json!({
            "v": 1,
            "kind": "tool",
            "call_id": "call-1",
            "result": huge,
        }));
        context.record.kind = TrajectoryKind::Tool;
        context.record.lane = TrajectoryLane::Tools;
        let response = project_detail(&context, TrajectoryDetailSection::Result, None, None);
        assert_detail_within_wire_limit(&response);
        let TrajectoryResponse::Detail {
            content,
            has_more,
            next_cursor,
            ..
        } = response
        else {
            panic!("expected detail");
        };
        assert!(has_more);
        assert!(next_cursor.is_some());
        assert!(
            content.byte_length <= u64::from(wakuwaku_protocol::TRAJECTORY_DETAIL_WINDOW_BYTES)
        );
        assert!(content.total_bytes > content.byte_length);
    }
}
