//! Incremental harness events.
//!
//! Events carry only deltas and indices — never a full partial message
//! snapshot. Tool completion and tool-call end share the same `Arc` stored
//! on the session transcript / assistant content.

use std::sync::Arc;

use crate::model::{StopReason, ToolCall, ToolResult, Usage};

/// Streaming events for one assistant response.
#[derive(Debug)]
pub enum StreamEvent {
    Start,
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
    },
    ToolCallStart {
        content_index: usize,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    /// Finalized tool call. Same `Arc` as the assistant content block.
    ToolCallEnd {
        content_index: usize,
        tool_call: Arc<ToolCall>,
    },
    Done {
        usage: Usage,
        stop_reason: StopReason,
    },
    Failed {
        usage: Usage,
        stop_reason: StopReason,
        error_message: Option<String>,
    },
}

/// Agent-level events emitted by the loop.
#[derive(Debug)]
pub enum AgentEvent {
    RunStarted,
    TurnStarted,
    SteeringInjected {
        id: u64,
    },
    Assistant(StreamEvent),
    AssistantDone,
    ToolStarted {
        tool_call_id: String,
        tool_name: String,
    },
    /// Same `Arc<ToolResult>` later stored on the session transcript.
    ToolFinished {
        result: Arc<ToolResult>,
    },
    TurnFinished,
    RunEnded {
        stop_reason: StopReason,
        error_message: Option<String>,
    },
}

impl AgentEvent {
    pub fn describes_tool(&self) -> Option<&str> {
        match self {
            AgentEvent::ToolStarted { tool_name, .. } => Some(tool_name),
            AgentEvent::ToolFinished { result } => Some(result.tool_name.as_str()),
            _ => None,
        }
    }
}
