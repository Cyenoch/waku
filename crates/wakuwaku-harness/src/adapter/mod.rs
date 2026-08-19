//! Wire adapters: shared streaming scratch plus the three format modules.

pub(crate) mod anthropic;
pub(crate) mod chat;
pub(crate) mod responses;

use crate::error::HarnessError;
use crate::events::StreamEvent;
use crate::model::{
    ApiFormat, AssistantMessage, ContentBlock, PromptContext, ProviderModel, RequestOptions,
    StopReason, TextBlock, ThinkingBlock, ToolCall,
};
use crate::transform::strip_foreign_dialects;
use serde_json::Value;
use std::sync::Arc;

/// Mutable scratch assembling one assistant response while streaming.
///
/// The scratch owns the in-progress message; adapters mutate it through these
/// methods which return the events to forward. Nothing partial is cloned out.
#[derive(Debug)]
pub struct AssistantScratch {
    pub msg: AssistantMessage,
    /// Argument fragment buffers by content index (wire-indexed tool calls).
    tool_json: Vec<(usize, crate::transform::StreamingJsonParser)>,
    /// Blocks for which a signature delta has already been received. A
    /// signature embedded in `content_block_start` is metadata, while the
    /// first delta supplies the canonical signature and subsequent deltas are
    /// appended to it.
    signature_delta_blocks: Vec<usize>,
}

impl AssistantScratch {
    pub fn new(model: &str, provider: &str) -> Self {
        AssistantScratch {
            msg: AssistantMessage {
                content: Vec::new(),
                model: model.to_string(),
                provider: provider.to_string(),
                response_id: None,
                usage: Default::default(),
                stop_reason: StopReason::Pending,
                error_message: None,
            },
            tool_json: Vec::new(),
            signature_delta_blocks: Vec::new(),
        }
    }

    pub fn set_response_id(&mut self, id: &str) {
        self.msg.response_id = Some(id.to_string());
    }

    fn push_block(&mut self, block: ContentBlock) -> usize {
        self.msg.content.push(block);
        self.msg.content.len() - 1
    }

    pub fn open_text(&mut self) -> (usize, StreamEvent) {
        let i = self.push_block(ContentBlock::Text(TextBlock {
            text: String::new(),
            signature: None,
        }));
        (i, StreamEvent::TextStart { content_index: i })
    }

    pub fn open_thinking(
        &mut self,
        signature: Option<String>,
        redacted: bool,
    ) -> (usize, StreamEvent) {
        let i = self.push_block(ContentBlock::Thinking(ThinkingBlock {
            thinking: String::new(),
            signature,
            redacted,
        }));
        (i, StreamEvent::ThinkingStart { content_index: i })
    }

    pub fn open_tool_call(&mut self, id: &str, name: &str) -> (usize, StreamEvent) {
        let i = self.push_block(ContentBlock::ToolCall(Arc::new(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: Value::Object(Default::default()),
            thought_signature: None,
        })));
        self.tool_json
            .push((i, crate::transform::StreamingJsonParser::new()));
        (i, StreamEvent::ToolCallStart { content_index: i })
    }

    pub fn text_delta(&mut self, idx: usize, delta: &str) -> StreamEvent {
        if let Some(ContentBlock::Text(t)) = self.msg.content.get_mut(idx) {
            t.text.push_str(delta);
        }
        StreamEvent::TextDelta {
            content_index: idx,
            delta: delta.to_string(),
        }
    }

    /// Replace streamed text with the authoritative final item content.
    pub fn replace_text(&mut self, idx: usize, text: &str) {
        if let Some(ContentBlock::Text(t)) = self.msg.content.get_mut(idx) {
            t.text.clear();
            t.text.push_str(text);
        }
    }

    pub fn thinking_delta(&mut self, idx: usize, delta: &str) -> StreamEvent {
        if let Some(ContentBlock::Thinking(t)) = self.msg.content.get_mut(idx) {
            t.thinking.push_str(delta);
        }
        StreamEvent::ThinkingDelta {
            content_index: idx,
            delta: delta.to_string(),
        }
    }

    /// Append a raw wire fragment for a tool call and refresh its lenient
    /// argument parse. Returns the delta event.
    pub fn replace_tool_call_args(&mut self, idx: usize, arguments: &str) -> StreamEvent {
        for (i, parser) in &mut self.tool_json {
            if *i == idx {
                *parser = crate::transform::StreamingJsonParser::new();
                parser.push(arguments);
            }
        }
        StreamEvent::ToolCallDelta {
            content_index: idx,
            delta: arguments.to_string(),
        }
    }

    pub fn tool_call_delta(&mut self, idx: usize, fragment: &str) -> StreamEvent {
        for (i, parser) in &mut self.tool_json {
            if *i == idx {
                parser.push(fragment);
            }
        }
        StreamEvent::ToolCallDelta {
            content_index: idx,
            delta: fragment.to_string(),
        }
    }

    pub fn set_tool_call_id(&mut self, idx: usize, id: &str) {
        if let Some(ContentBlock::ToolCall(c)) = self.msg.content.get_mut(idx)
            && c.id.is_empty()
        {
            Arc::make_mut(c).id = id.to_string();
        }
    }

    pub fn set_tool_call_name(&mut self, idx: usize, name: &str) {
        if let Some(ContentBlock::ToolCall(c)) = self.msg.content.get_mut(idx)
            && c.name.is_empty()
        {
            Arc::make_mut(c).name = name.to_string();
        }
    }

    pub fn set_tool_call_composite_id(&mut self, idx: usize, id: &str) {
        if let Some(ContentBlock::ToolCall(c)) = self.msg.content.get_mut(idx) {
            Arc::make_mut(c).id = id.to_string();
        }
    }

    /// Finalize a tool call: strict parse of accumulated fragments. Invalid
    /// JSON is surfaced as a stream error instead of being silently converted
    /// into an executable string argument.
    pub fn end_tool_call(&mut self, idx: usize) -> Result<StreamEvent, HarnessError> {
        let pos = self.tool_json.iter().position(|(i, _)| *i == idx);
        let Some(pos) = pos else {
            return Err(HarnessError::Malformed {
                format: "adapter",
                detail: format!("toolcall end without open scratch at index {idx}"),
            });
        };
        let mut parser = self.tool_json.swap_remove(pos).1;
        let args = if parser.raw().trim().is_empty() {
            Value::Object(Default::default())
        } else {
            parser.finish().map_err(|detail| HarnessError::Malformed {
                format: "tool-call",
                detail,
            })?
        };
        let call = match self.msg.content.get_mut(idx) {
            Some(ContentBlock::ToolCall(c)) => {
                Arc::make_mut(c).arguments = args;
                Arc::clone(c)
            }
            _ => {
                return Err(HarnessError::Malformed {
                    format: "adapter",
                    detail: "toolcall end without a tool block".to_string(),
                });
            }
        };
        Ok(StreamEvent::ToolCallEnd {
            content_index: idx,
            tool_call: call,
        })
    }

    pub fn text_end(&mut self, idx: usize) -> StreamEvent {
        StreamEvent::TextEnd { content_index: idx }
    }
    pub fn thinking_end(&mut self, idx: usize) -> StreamEvent {
        StreamEvent::ThinkingEnd { content_index: idx }
    }

    /// Start a wire signature sequence with its first delta, then append later
    /// fragments. This intentionally replaces a provisional start-block value.
    pub fn append_thinking_signature(&mut self, idx: usize, sig: &str) {
        if let Some(ContentBlock::Thinking(t)) = self.msg.content.get_mut(idx) {
            if self.signature_delta_blocks.contains(&idx) {
                t.signature.get_or_insert_with(String::new).push_str(sig);
            } else {
                t.signature = Some(sig.to_string());
                self.signature_delta_blocks.push(idx);
            }
        }
    }

    pub fn set_thinking_signature(&mut self, idx: usize, sig: &str) {
        if let Some(ContentBlock::Thinking(t)) = self.msg.content.get_mut(idx) {
            t.signature = Some(sig.to_string());
        }
    }

    pub fn set_text_signature(&mut self, idx: usize, sig: &str) {
        if let Some(ContentBlock::Text(t)) = self.msg.content.get_mut(idx) {
            t.signature = Some(sig.to_string());
        }
    }

    pub fn usage_mut(&mut self) -> &mut crate::model::Usage {
        &mut self.msg.usage
    }

    fn take_owned(&mut self) -> AssistantScratch {
        let model = self.msg.model.clone();
        let provider = self.msg.provider.clone();
        std::mem::replace(self, AssistantScratch::new(&model, &provider))
    }

    pub fn finish(
        mut self,
        reason: StopReason,
        error: Option<String>,
    ) -> (Box<AssistantMessage>, StreamEvent) {
        self.msg.stop_reason = reason;
        self.msg.error_message = error;
        let event = match reason {
            StopReason::Error | StopReason::Aborted => StreamEvent::Failed {
                usage: self.msg.usage,
                stop_reason: reason,
                error_message: self.msg.error_message.clone(),
            },
            _ => StreamEvent::Done {
                usage: self.msg.usage,
                stop_reason: reason,
            },
        };
        (Box::new(self.msg), event)
    }

    /// Finish a response while retaining a valid empty scratch for the
    /// adapter's borrowed API.
    pub fn finish_in_place(
        &mut self,
        reason: StopReason,
        error: Option<String>,
    ) -> (Box<AssistantMessage>, StreamEvent) {
        self.take_owned().finish(reason, error)
    }

    pub fn fail(self, error: HarnessError) -> (Box<AssistantMessage>, StreamEvent) {
        let reason = match error {
            HarnessError::Cancelled => StopReason::Aborted,
            _ => StopReason::Error,
        };
        let msg = error.to_string();
        self.finish(reason, Some(msg))
    }

    pub fn fail_in_place(&mut self, error: HarnessError) -> (Box<AssistantMessage>, StreamEvent) {
        self.take_owned().fail(error)
    }
}

/// What an SSE payload did to the stream.
#[derive(Debug)]
pub enum PayloadOutcome {
    /// Events to forward; streaming continues.
    Events(Vec<StreamEvent>),
    /// Terminal success or failure with all lifecycle events and the finalized message.
    Terminal(Box<AssistantMessage>, Vec<StreamEvent>),
}

/// Per-request dispatch entry: builds the wire body for a format.
pub fn build_body(
    format: ApiFormat,
    ctx: &PromptContext,
    target: &ProviderModel,
    opts: &RequestOptions,
) -> Result<Value, HarnessError> {
    let mut messages = ctx.messages.clone();
    let _stripped = strip_foreign_dialects(&mut messages, target, format);
    let request_ctx = PromptContext {
        system_prompt: ctx.system_prompt.clone(),
        messages,
        tools: ctx.tools.clone(),
        provider_model: Some(target.clone()),
    };
    match format {
        ApiFormat::OpenAiResponses => responses::build_body(&request_ctx, &target.model, opts),
        ApiFormat::OpenAiChat => chat::build_body(&request_ctx, &target.model, opts),
        ApiFormat::Anthropic => anthropic::build_body(&request_ctx, &target.model, opts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssistantMessage, ContentBlock, Message, StopReason, TextBlock, ThinkingBlock, ToolCall,
        ToolResult, ToolResultPart,
    };
    use std::sync::Arc;
    use wakuwaku_provider::ProviderId;

    fn target(provider: &str, model: &str) -> ProviderModel {
        ProviderModel {
            provider: ProviderId::new(provider),
            model: model.into(),
        }
    }

    fn ctx(messages: Vec<Message>) -> PromptContext {
        PromptContext {
            system_prompt: None,
            messages,
            tools: Vec::new(),
            provider_model: None,
        }
    }

    fn assistant(provider: &str, model: &str, content: Vec<ContentBlock>) -> Message {
        Message::Assistant(Arc::new(AssistantMessage {
            content,
            model: model.into(),
            provider: provider.into(),
            response_id: None,
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
        }))
    }

    #[test]
    fn chat_build_body_demotes_anthropic_thinking_and_leaves_store_untouched() {
        let stored = ctx(vec![assistant(
            "anthropic",
            "claude-opus",
            vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "secret plan".into(),
                    signature: Some("anth-sig".into()),
                    redacted: false,
                }),
                ContentBlock::Text(TextBlock {
                    text: "hello".into(),
                    signature: None,
                }),
            ],
        )]);
        let before = stored.messages.clone();
        let body = build_body(
            ApiFormat::OpenAiChat,
            &stored,
            &target("openai-chat", "gpt-4.1"),
            &RequestOptions::default(),
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert!(assistant.get("reasoning_content").is_none());
        let content = assistant["content"].as_str().unwrap();
        assert!(content.contains("secret plan"));
        assert!(content.contains("hello"));
        assert_eq!(stored.messages.len(), before.len());
        let Message::Assistant(original) = &stored.messages[0] else {
            panic!("stored assistant");
        };
        assert!(matches!(
            &original.content[0],
            ContentBlock::Thinking(thinking)
                if thinking.signature.as_deref() == Some("anth-sig")
        ));
    }

    #[test]
    fn chat_build_body_same_model_replays_reasoning_content() {
        let stored = ctx(vec![assistant(
            "openai-chat",
            "gpt-4.1",
            vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: "keep".into(),
                signature: Some("reasoning_content".into()),
                redacted: false,
            })],
        )]);
        let body = build_body(
            ApiFormat::OpenAiChat,
            &stored,
            &target("openai-chat", "gpt-4.1"),
            &RequestOptions::default(),
        )
        .unwrap();
        let assistant = &body["messages"][0];
        assert_eq!(assistant["reasoning_content"], "keep");
        assert_eq!(assistant["content"], "");
    }

    #[test]
    fn chat_build_body_pairs_normalized_composite_tool_ids() {
        let stored = ctx(vec![
            assistant(
                "openai-responses",
                "gpt-5",
                vec![ContentBlock::ToolCall(Arc::new(ToolCall {
                    id: "call_1|fc_a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a"}),
                    thought_signature: Some("tsig".into()),
                }))],
            ),
            Message::ToolResult(Arc::new(ToolResult {
                tool_call_id: "call_1|fc_a".into(),
                tool_name: "read".into(),
                content: vec![ToolResultPart::Text("ok".into())],
                is_error: false,
                details: None,
            })),
        ]);
        let body = build_body(
            ApiFormat::OpenAiChat,
            &stored,
            &target("openai-chat", "gpt-4.1"),
            &RequestOptions::default(),
        )
        .unwrap();
        let messages = body["messages"].as_array().unwrap();
        let call_id = messages[0]["tool_calls"][0]["id"].as_str().unwrap();
        let result_id = messages[1]["tool_call_id"].as_str().unwrap();
        assert_eq!(call_id, "call_1_fc_a");
        assert_eq!(result_id, call_id);
        assert!(messages[0].get("thought_signature").is_none());
    }

    #[test]
    fn responses_build_body_drops_foreign_reasoning_and_item_ids() {
        let stored = ctx(vec![assistant(
            "anthropic",
            "claude-opus",
            vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "draft".into(),
                    signature: Some("anth-sig".into()),
                    redacted: false,
                }),
                ContentBlock::Text(TextBlock {
                    text: "hello".into(),
                    signature: Some(r#"{"v":1,"id":"msg_foreign"}"#.into()),
                }),
            ],
        )]);
        let body = build_body(
            ApiFormat::OpenAiResponses,
            &stored,
            &target("openai-responses", "gpt-5"),
            &RequestOptions::default(),
        )
        .unwrap();
        let input = body["input"].as_array().unwrap();
        assert!(
            input
                .iter()
                .all(|item| item["type"] != "reasoning" && item.get("signature").is_none())
        );
        let message = input.iter().find(|item| item["type"] == "message").unwrap();
        assert_ne!(message["id"], "msg_foreign");
    }

    #[test]
    fn responses_build_body_strips_real_xai_snapshot_metadata_cross_provider() {
        let stored = ctx(vec![assistant(
            "xai-oauth",
            "grok-4.5",
            vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "provider reasoning".into(),
                    signature: Some(
                        r#"{"id":"rs_50ac","status":"completed","type":"reasoning"}"#.into(),
                    ),
                    redacted: false,
                }),
                ContentBlock::Text(TextBlock {
                    text: "prior answer".into(),
                    signature: Some(r#"{"id":"msg_50ac","v":1}"#.into()),
                }),
            ],
        )]);

        let body = build_body(
            ApiFormat::OpenAiResponses,
            &stored,
            &target("opencode-go", "grok-4.5"),
            &RequestOptions::default(),
        )
        .unwrap();

        let input = body["input"].as_array().unwrap();
        assert!(input.iter().all(|item| item["type"] != "reasoning"));
        let message = input.iter().find(|item| item["type"] == "message").unwrap();
        assert_ne!(message["id"], "msg_50ac");
        let Message::Assistant(original) = &stored.messages[0] else {
            panic!("stored assistant");
        };
        assert!(matches!(
            &original.content[0],
            ContentBlock::Thinking(thinking) if thinking.signature.as_deref().is_some()
        ));
        assert!(matches!(
            &original.content[1],
            ContentBlock::Text(text) if text.signature.as_deref().is_some()
        ));
    }

    #[test]
    fn responses_build_body_same_model_replays_encrypted_reasoning() {
        let reasoning = serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "blob"
        });
        let stored = ctx(vec![assistant(
            "openai-responses",
            "gpt-5",
            vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: String::new(),
                signature: Some(reasoning.to_string()),
                redacted: false,
            })],
        )]);
        let body = build_body(
            ApiFormat::OpenAiResponses,
            &stored,
            &target("openai-responses", "gpt-5"),
            &RequestOptions::default(),
        )
        .unwrap();
        assert_eq!(body["input"][0], reasoning);
    }

    #[test]
    fn anthropic_build_body_replays_same_model_thinking_signature() {
        let stored = ctx(vec![assistant(
            "anthropic",
            "claude-opus",
            vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: "draft".into(),
                signature: Some("sig-1".into()),
                redacted: false,
            })],
        )]);
        let body = build_body(
            ApiFormat::Anthropic,
            &stored,
            &target("anthropic", "claude-opus"),
            &RequestOptions::default(),
        )
        .unwrap();
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "thinking");
        assert_eq!(block["signature"], "sig-1");
        assert_eq!(block["thinking"], "draft");
    }

    #[test]
    fn anthropic_build_body_demotes_foreign_thinking() {
        let stored = ctx(vec![assistant(
            "openai-chat",
            "gpt-4.1",
            vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: "draft".into(),
                signature: Some("reasoning_content".into()),
                redacted: false,
            })],
        )]);
        let body = build_body(
            ApiFormat::Anthropic,
            &stored,
            &target("anthropic", "claude-opus"),
            &RequestOptions::default(),
        )
        .unwrap();
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "text");
        assert_eq!(block["text"], "draft");
        assert!(block.get("signature").is_none());
    }
}
