//! Runtime-native LLM harness for Waku.
//!
//! The crate exposes a small deep interface: [`Harness`] and [`Session`] own
//! the agent loop, [`ModelProvider`] is the HTTP provider seam, and [`Tool`]
//! is the tool seam. Wire adapters, SSE framing, and transcript repair remain
//! implementation details so callers cannot accidentally depend on provider
//! internals.
//!
//! temp/pi served as the behavioral specification; this crate is a Rust-native
//! design, not a translation.

mod adapter;
mod agent;
mod cancel;
mod error;
mod events;
mod http;
mod model;
mod models;
mod provider;
mod sse;
mod tools;
mod transform;

pub use agent::{
    Budget, Harness, QueueMode, RunOutcome, Session, SessionCheckpoint, SessionSnapshot,
    SessionSteering, estimate_tokens,
};
pub use cancel::CancelToken;
pub use error::HarnessError;
pub use events::{AgentEvent, StreamEvent, TraceBuffer, TraceEvent, TraceSink};
pub use http::{HttpProvider, ModelProvider, RetryPolicy, SharedProvider};
pub use model::{
    ApiFormat, AssistantMessage, ContentBlock, Message, PromptContext, ProviderModel,
    RequestOptions, StopReason, TextBlock, ThinkingBlock, ToolCall, ToolResult, ToolResultPart,
    ToolSchema, Usage, UserMessage, UserPart,
};
pub use models::{auth_headers, models_url, models_url_for, parse_models_payload};
pub use provider::{Auth, ExtraHeaders, ProviderConfig, ProviderRequest, Providers};
pub use tools::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, ApprovalTool, ExecOutcome, ExecutionContext,
    ExecutionMode, Tool, ToolContext, ToolError, ToolSpec, edit::EditTool, list::ListTool,
    read::ReadTool, search::SearchTool, shell::ShellTool, write::WriteTool,
};
