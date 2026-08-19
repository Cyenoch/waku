//! Incremental harness events.
//!
//! Events carry only deltas and indices — never a full partial message
//! snapshot. Tool completion and tool-call end share the same `Arc` stored
//! on the session transcript / assistant content.
//!
//! [`TraceEvent`] is in-process only. It must not be forwarded through
//! `DriverEvent`, EventBridge, or any wire codec.

use std::sync::Arc;
use std::time::Instant;

use crate::model::{AssistantMessage, StopReason, ToolCall, ToolResult, Usage};

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

/// In-process observation of one drive. Timestamps are `Instant`s captured at
/// the semantic event; consumers convert to Unix ms with a run anchor.
#[derive(Debug)]
pub enum TraceEvent {
    PromptPrepared {
        system_prompt: Option<String>,
        tools_json: Arc<str>,
        options_json: Arc<str>,
        model_hint: String,
    },
    RequestStart {
        visible_turn: usize,
        step: usize,
        started_at: Instant,
    },
    RequestFirstToken {
        visible_turn: usize,
        step: usize,
        at: Instant,
    },
    RequestFailed {
        visible_turn: usize,
        step: usize,
        failed_at: Instant,
        error: String,
    },
    SteeringInjected {
        id: u64,
    },
    ToolExecution {
        call_id: String,
        started_at: Instant,
        finished_at: Instant,
        result_preview: Arc<str>,
    },
    /// Same `Arc` stored on `Message::Assistant`.
    AssistantDone(Arc<AssistantMessage>),
}

/// Receives [`TraceEvent`]s on the drive thread. Implementations must not do
/// I/O; concurrent tool futures never share this sink.
pub trait TraceSink: Send {
    fn emit(&mut self, event: TraceEvent);
}

impl TraceSink for () {
    fn emit(&mut self, _: TraceEvent) {}
}

impl<T: TraceSink + ?Sized> TraceSink for &mut T {
    fn emit(&mut self, event: TraceEvent) {
        (**self).emit(event);
    }
}

/// Growable in-process buffer used by the embedded driver and tests.
#[derive(Debug, Default)]
pub struct TraceBuffer {
    events: Vec<TraceEvent>,
}

impl TraceBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn events(&self) -> &[TraceEvent] {
        &self.events
    }

    pub fn drain(&mut self) -> Vec<TraceEvent> {
        std::mem::take(&mut self.events)
    }
}

impl TraceSink for TraceBuffer {
    fn emit(&mut self, event: TraceEvent) {
        self.events.push(event);
    }
}
