//! Unified runtime-native model shared by every adapter and the agent loop.
//!
//! Closed enums describe the wire protocols and stop reasons; ownership moves
//! a message from streaming scratch to finalized transcript without cloning
//! full partial snapshots per event.

use serde_json::Value;
use std::sync::Arc;

pub use wakuwaku_provider::ApiFormat;
use wakuwaku_provider::ProviderId;

/// Terminal outcome of an assistant response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TextBlock {
    pub text: String,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThinkingBlock {
    pub thinking: String,
    pub signature: Option<String>,
    pub redacted: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// Composite id when the wire format splits call and item identity
    /// (`call_id|item_id`); bare otherwise.
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ContentBlock {
    Text(TextBlock),
    Thinking(ThinkingBlock),
    ToolCall(Arc<ToolCall>),
}

impl ContentBlock {
    pub fn text(t: impl Into<String>) -> Self {
        ContentBlock::Text(TextBlock {
            text: t.into(),
            signature: None,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
}

impl Usage {
    pub fn derived_total(input: u64, output: u64, cache_read: u64, cache_write: u64) -> u64 {
        input
            .saturating_add(output)
            .saturating_add(cache_read)
            .saturating_add(cache_write)
    }
}

/// A user message. `parts` keeps text and images interleaved.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UserMessage {
    pub parts: Vec<UserPart>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum UserPart {
    Text(String),
    Image { mime_type: String, data_b64: String },
}

impl UserMessage {
    pub fn text(t: impl Into<String>) -> Self {
        UserMessage {
            parts: vec![UserPart::Text(t.into())],
        }
    }

    pub fn text_of(parts: &[UserPart]) -> String {
        let mut s = String::new();
        for p in parts {
            if let UserPart::Text(t) = p {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(t);
            }
        }
        s
    }
}

impl From<String> for UserMessage {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

impl From<&str> for UserMessage {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub provider: String,
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
}

impl AssistantMessage {
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolCall(c) => Some(c.as_ref()),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ToolResultPart>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<std::sync::Arc<serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ToolResultPart {
    Text(String),
    Image { mime_type: String, data_b64: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Message {
    User(UserMessage),
    Assistant(Arc<AssistantMessage>),
    ToolResult(Arc<ToolResult>),
}

impl Message {
    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            Message::Assistant(a) => Some(a.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Request-time target identity for same-model signature replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModel {
    pub provider: ProviderId,
    pub model: String,
}

#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub provider_model: Option<ProviderModel>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub reasoning: Option<String>,
    pub service_tier: Option<wakuwaku_provider::ServiceTier>,
    pub omit_sampling: bool,
    pub omit_reasoning_summary: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_arc_serde_matches_inner_shape() {
        let message = Message::Assistant(Arc::new(AssistantMessage {
            content: vec![ContentBlock::text("hi")],
            model: "m".into(),
            provider: "p".into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
        }));
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["Assistant"]["model"], "m");
        assert_eq!(value["Assistant"]["content"][0]["Text"]["text"], "hi");
        let restored: Message = serde_json::from_value(value).unwrap();
        let assistant = restored.as_assistant().expect("assistant");
        assert_eq!(assistant.model, "m");
        assert_eq!(assistant.provider, "p");
    }
}

