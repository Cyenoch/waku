//! Cross-adapter transcript normalization.
//!
//! Mirrors the provider-agnostic behaviors every wire format needs: orphan
//! tool-call repair, tool-call id sanitization, and image downgrade.

use crate::model::{Message, ToolResult, ToolResultPart};
use std::borrow::Cow;
use std::sync::Arc;

/// Borrow a transcript when no repair is needed, allocating only when a
/// synthetic result must be inserted. Adapters use this to avoid cloning a
/// long transcript on every request.
pub fn repaired_messages(messages: &[Message]) -> Cow<'_, [Message]> {
    if !needs_repair(messages) {
        Cow::Borrowed(messages)
    } else {
        Cow::Owned(repair_owned(messages.to_vec()))
    }
}

fn needs_repair(messages: &[Message]) -> bool {
    let mut index = 0;
    while index < messages.len() {
        let Message::Assistant(assistant) = &messages[index] else {
            index += 1;
            continue;
        };
        let calls: Vec<&str> = assistant
            .tool_calls()
            .map(|call| call.id.as_str())
            .collect();
        if calls.is_empty() {
            index += 1;
            continue;
        }
        let mut results = Vec::new();
        let mut cursor = index + 1;
        while let Some(Message::ToolResult(result)) = messages.get(cursor) {
            results.push(result.tool_call_id.as_str());
            cursor += 1;
        }
        if calls.iter().any(|call| !results.contains(call)) {
            return true;
        }
        index = cursor;
    }
    false
}

fn repair_owned(messages: Vec<Message>) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len() + 4);
    let mut index = 0;
    while index < messages.len() {
        let Some(message) = messages.get(index) else {
            break;
        };
        let Message::Assistant(assistant) = message else {
            out.push(messages[index].clone());
            index += 1;
            continue;
        };
        let calls: Vec<(String, String)> = assistant
            .tool_calls()
            .map(|call| (call.id.clone(), call.name.clone()))
            .collect();
        out.push(messages[index].clone());
        index += 1;
        if calls.is_empty() {
            continue;
        }

        let batch_start = index;
        while matches!(messages.get(index), Some(Message::ToolResult(_))) {
            index += 1;
        }
        let mut real_results: Vec<(String, Arc<ToolResult>)> = messages[batch_start..index]
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some((result.tool_call_id.clone(), result.clone())),
                _ => None,
            })
            .collect();

        // Emit one result per call in assistant source order. This is the
        // order provider protocols use to validate a tool batch.
        for (call_id, tool_name) in calls {
            if let Some(position) = real_results
                .iter()
                .position(|(result_id, _)| result_id == &call_id)
            {
                let (_, result) = real_results.remove(position);
                out.push(Message::ToolResult(result));
            } else {
                out.push(Message::ToolResult(synthetic_result(call_id, tool_name)));
            }
        }
        // Preserve unrelated results rather than silently dropping history.
        out.extend(
            real_results
                .into_iter()
                .map(|(_, result)| Message::ToolResult(result)),
        );
    }
    out
}

fn synthetic_result(tool_call_id: String, tool_name: String) -> Arc<ToolResult> {
    Arc::new(ToolResult {
        tool_call_id,
        tool_name,
        content: vec![ToolResultPart::Text(
            "(no tool output: call was not completed)".into(),
        )],
        is_error: true,
        details: None,
    })
}

/// Character-and-length sanitizer for provider id constraints.
/// `max_len` is the wire limit; composite `call|item` ids retain both identity
/// parts and hash the complete original id whenever truncation is needed.
pub fn normalize_tool_call_id(id: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let sanitized = if let Some((call, item)) = id.split_once('|') {
        format!("{}_{}", sanitize(call), sanitize(item))
    } else {
        sanitize(id)
    };
    if sanitized.chars().count() <= max_len {
        return sanitized;
    }
    let hash = short_hash(id);
    if max_len <= hash.len() {
        return hash.chars().take(max_len).collect();
    }
    let prefix_len = max_len - hash.len() - 1;
    let prefix: String = sanitized.chars().take(prefix_len).collect();
    if prefix.is_empty() {
        return hash.chars().take(max_len).collect();
    }
    format!("{prefix}_{hash}")
}

/// FNV-1a 64-bit short hash, hex-encoded and stable across runs.
pub fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Streaming-tolerant JSON parser for tool-call argument fragments.
#[derive(Default, Debug)]
pub struct StreamingJsonParser {
    buf: String,
}

impl StreamingJsonParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, fragment: &str) {
        self.buf.push_str(fragment);
    }

    /// The accumulated raw text.
    pub fn raw(&self) -> &str {
        &self.buf
    }

    /// Strict final parse of everything accumulated.
    pub fn finish(&mut self) -> Result<serde_json::Value, String> {
        let raw = std::mem::take(&mut self.buf);
        serde_json::from_str(&raw).map_err(|e| format!("invalid tool arguments: {e}"))
    }
}

#[cfg(test)]
fn repair_orphan_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    repair_owned(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, ContentBlock, TextBlock, ToolCall, UserMessage};

    fn assistant_with_calls(ids: &[&str]) -> Message {
        Message::Assistant(AssistantMessage {
            content: ids
                .iter()
                .map(|id| {
                    ContentBlock::ToolCall(Arc::new(ToolCall {
                        id: id.to_string(),
                        name: "t".into(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    }))
                })
                .chain(std::iter::once(ContentBlock::Text(TextBlock {
                    text: "hi".into(),
                    signature: None,
                })))
                .collect(),
            model: "m".into(),
            provider: "p".into(),
            response_id: None,
            usage: Default::default(),
            stop_reason: crate::model::StopReason::ToolUse,
            error_message: None,
        })
    }

    fn result_for(id: &str) -> Message {
        Message::ToolResult(Arc::new(ToolResult {
            tool_call_id: id.into(),
            tool_name: "t".into(),
            content: vec![ToolResultPart::Text("ok".into())],
            is_error: false,
            details: None,
        }))
    }

    #[test]
    fn orphan_calls_get_synthetic_results_in_batch_order() {
        let msgs = vec![
            Message::User(UserMessage::text("go")),
            assistant_with_calls(&["call_1|fc_a", "call_2|fc_b"]),
            result_for("call_2|fc_b"),
        ];
        let out = repair_orphan_tool_calls(msgs);
        assert!(
            matches!(out[2], Message::ToolResult(ref r) if r.is_error && r.tool_call_id == "call_1|fc_a")
        );
        assert!(
            matches!(out[3], Message::ToolResult(ref r) if !r.is_error && r.tool_call_id == "call_2|fc_b")
        );
    }

    #[test]
    fn trailing_unresolved_calls_are_patched() {
        let msgs = vec![
            Message::User(UserMessage::text("go")),
            assistant_with_calls(&["c1"]),
        ];
        let out = repair_orphan_tool_calls(msgs);
        assert!(matches!(out.last(), Some(Message::ToolResult(_))));
    }

    #[test]
    fn id_normalization_hashes_bare_and_composite_overflow() {
        assert_eq!(
            normalize_tool_call_id("call_1|fc_item", 40),
            "call_1_fc_item"
        );
        let long_item = "i".repeat(80);
        let id = format!("call_1|{long_item}");
        let n = normalize_tool_call_id(&id, 40);
        assert!(n.chars().count() <= 40);
        assert!(n.starts_with("call_1_"));
        let a = normalize_tool_call_id(&"a".repeat(100), 40);
        let b = normalize_tool_call_id(&format!("{}b", "a".repeat(99)), 40);
        assert_ne!(a, b);
        assert_eq!(normalize_tool_call_id("anything", 0), "");
        let bad = normalize_tool_call_id("we!ird/id", 40);
        assert_eq!(bad, "we_ird_id");
    }

    #[test]
    fn streaming_json_finish_parses_once() {
        let mut p = StreamingJsonParser::new();
        p.push(r#"{"path":"a"#);
        p.push(r#"b.c","line":1}"#);
        let v = p.finish().unwrap();
        assert_eq!(v["path"], "ab.c");
        assert_eq!(v["line"], 1);
    }
}
