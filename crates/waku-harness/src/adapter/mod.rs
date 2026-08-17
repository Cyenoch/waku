//! Wire adapters: shared streaming scratch plus the three format modules.

pub(crate) mod anthropic;
pub(crate) mod chat;
pub(crate) mod responses;

use crate::error::HarnessError;
use crate::events::StreamEvent;
use crate::model::{
    ApiFormat, AssistantMessage, ContentBlock, RequestOptions, StopReason, TextBlock,
    ThinkingBlock, ToolCall,
};
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
    ctx: &crate::model::PromptContext,
    model: &str,
    opts: &RequestOptions,
) -> Result<Value, HarnessError> {
    match format {
        ApiFormat::OpenAiResponses => responses::build_body(ctx, model, opts),
        ApiFormat::OpenAiChat => chat::build_body(ctx, model, opts),
        ApiFormat::Anthropic => anthropic::build_body(ctx, model, opts),
    }
}
