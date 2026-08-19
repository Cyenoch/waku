use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use wakuwaku_harness::{
    AssistantMessage, CancelToken, ContentBlock, ExecOutcome, ExecutionContext, Harness,
    HarnessError, Message, ModelProvider, PromptContext, RequestOptions, RunOutcome, Session,
    StopReason, StreamEvent, TextBlock, ThinkingBlock, Tool, ToolCall, ToolError, ToolSpec,
    TraceBuffer, TraceEvent, Usage,
};

enum Script {
    Complete {
        events: Vec<StreamEvent>,
        message: AssistantMessage,
    },
    Fail {
        events: Vec<StreamEvent>,
        error: HarnessError,
    },
}

struct ScriptedProvider {
    scripts: Mutex<VecDeque<Script>>,
}

impl ScriptedProvider {
    fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
        })
    }
}

impl ModelProvider for ScriptedProvider {
    fn complete<'a>(
        &'a self,
        _ctx: &'a PromptContext,
        _opts: &'a RequestOptions,
        _model: Option<&'a str>,
        _cancel: CancelToken,
        sink: &'a mut (dyn FnMut(StreamEvent) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<AssistantMessage, HarnessError>> + Send + 'a>> {
        let script = self.scripts.lock().pop_front().expect("scripted response");
        Box::pin(async move {
            match script {
                Script::Complete { events, message } => {
                    emit_scripted(sink, events, message.usage, message.stop_reason);
                    Ok(message)
                }
                Script::Fail { events, error } => {
                    emit_scripted(sink, events, Usage::default(), StopReason::Error);
                    Err(error)
                }
            }
        })
    }
}

fn emit_scripted(
    sink: &mut (dyn FnMut(StreamEvent) + Send),
    events: Vec<StreamEvent>,
    usage: Usage,
    stop_reason: StopReason,
) {
    if events.is_empty() {
        sink(StreamEvent::Start);
        sink(StreamEvent::Done { usage, stop_reason });
        return;
    }
    for event in events {
        sink(event);
    }
}

struct DelayTool {
    name: &'static str,
    spec: ToolSpec,
    delay: Duration,
}

impl DelayTool {
    fn new(name: &'static str, delay: Duration) -> Self {
        Self {
            name,
            spec: ToolSpec {
                name: name.into(),
                description: "test tool".into(),
                parameters: json!({"type": "object"}),
                required: Vec::new(),
            },
            delay,
        }
    }
}

impl Tool for DelayTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(
        &'a self,
        _call: &'a ToolCall,
        exec: ExecutionContext<'a>,
    ) -> BoxFuture<'a, Result<ExecOutcome, ToolError>> {
        Box::pin(async move {
            exec.cancel
                .race_delay(self.delay)
                .await
                .map_err(|_| ToolError::Cancelled)?;
            Ok(ExecOutcome::text(self.name))
        })
    }
}

fn assistant_text(text: &str, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextBlock {
            text: text.into(),
            signature: None,
        })],
        model: "scripted".into(),
        provider: "scripted".into(),
        response_id: None,
        usage: Usage::default(),
        stop_reason,
        error_message: None,
    }
}

fn assistant_tools(ids: &[(&str, &str)], stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content: ids
            .iter()
            .map(|(id, name)| {
                ContentBlock::ToolCall(Arc::new(ToolCall {
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: json!({}),
                    thought_signature: None,
                }))
            })
            .collect(),
        model: "scripted".into(),
        provider: "scripted".into(),
        response_id: None,
        usage: Usage::default(),
        stop_reason,
        error_message: None,
    }
}

fn request_starts(traces: &[TraceEvent]) -> Vec<(usize, usize)> {
    traces
        .iter()
        .filter_map(|event| match event {
            TraceEvent::RequestStart {
                visible_turn, step, ..
            } => Some((*visible_turn, *step)),
            _ => None,
        })
        .collect()
}

fn first_tokens(traces: &[TraceEvent]) -> Vec<(usize, usize)> {
    traces
        .iter()
        .filter_map(|event| match event {
            TraceEvent::RequestFirstToken {
                visible_turn, step, ..
            } => Some((*visible_turn, *step)),
            _ => None,
        })
        .collect()
}

fn tool_ids(traces: &[TraceEvent]) -> Vec<String> {
    traces
        .iter()
        .filter_map(|event| match event {
            TraceEvent::ToolExecution { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

async fn run_with_trace(
    harness: &Harness,
    session: &mut Session,
    prompt: &str,
    traces: &mut TraceBuffer,
) -> Result<RunOutcome, HarnessError> {
    harness
        .run(session, prompt, CancelToken::new(), |_| {}, traces)
        .await
}

#[tokio::test]
async fn one_visible_turn_with_tool_continuation_reports_steps_and_arc_identity() {
    let provider = ScriptedProvider::new(vec![
        Script::Complete {
            events: vec![
                StreamEvent::Start,
                StreamEvent::ToolCallStart { content_index: 0 },
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                },
            ],
            message: assistant_tools(
                &[("slow", "slow"), ("fast", "fast"), ("missing", "missing")],
                StopReason::ToolUse,
            ),
        },
        Script::Complete {
            events: vec![
                StreamEvent::Start,
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "done".into(),
                },
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                },
            ],
            message: assistant_text("done", StopReason::Stop),
        },
    ]);
    let harness = Harness::new(provider)
        .with_model("scripted-model")
        .with_request_options(RequestOptions {
            max_tokens: Some(128),
            ..RequestOptions::default()
        })
        .with_tools(vec![
            Arc::new(DelayTool::new("slow", Duration::from_millis(20))),
            Arc::new(DelayTool::new("fast", Duration::from_millis(1))),
        ]);
    let mut session = Session::new(Some("system prompt".into()));
    let mut traces = TraceBuffer::new();
    let outcome = run_with_trace(&harness, &mut session, "go", &mut traces)
        .await
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Completed));

    let events = traces.events();
    assert!(matches!(
        events.first(),
        Some(TraceEvent::PromptPrepared {
            system_prompt: Some(prompt),
            model_hint,
            tools_json,
            options_json,
        }) if prompt == "system prompt"
            && model_hint == "scripted-model"
            && tools_json.contains("slow")
            && tools_json.contains("fast")
            && options_json.contains("128")
            && !tools_json.contains("go")
    ));
    assert_eq!(request_starts(events), vec![(1, 1), (1, 2)]);
    assert_eq!(first_tokens(events), vec![(1, 1), (1, 2)]);

    let mut tool_ids = tool_ids(events);
    tool_ids.sort();
    assert_eq!(tool_ids, vec!["fast", "missing", "slow"]);
    for event in events {
        if let TraceEvent::ToolExecution {
            started_at,
            finished_at,
            result_preview,
            call_id,
        } = event
        {
            assert!(finished_at >= started_at, "{call_id}");
            if call_id == "missing" {
                assert!(result_preview.contains("unknown tool"));
            }
        }
    }

    let stored: Vec<_> = session
        .transcript()
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .collect();
    let done: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::AssistantDone(assistant) => Some(assistant),
            _ => None,
        })
        .collect();
    assert_eq!(stored.len(), 2);
    assert_eq!(done.len(), 2);
    assert!(Arc::ptr_eq(stored[0], done[0]));
    assert!(Arc::ptr_eq(stored[1], done[1]));

    let transcript_tools: Vec<_> = session
        .transcript()
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(transcript_tools, vec!["slow", "fast", "missing"]);
}

#[tokio::test]
async fn first_token_is_exact_and_once_for_each_edge() {
    async fn first_token_count(events: Vec<StreamEvent>, message: AssistantMessage) -> usize {
        let provider = ScriptedProvider::new(vec![Script::Complete { events, message }]);
        let harness = Harness::new(provider);
        let mut session = Session::new(None);
        let mut traces = TraceBuffer::new();
        run_with_trace(&harness, &mut session, "go", &mut traces)
            .await
            .unwrap();
        first_tokens(traces.events()).len()
    }

    assert_eq!(
        first_token_count(
            vec![
                StreamEvent::Start,
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: String::new(),
                },
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "hi".into(),
                },
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: " there".into(),
                },
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                },
            ],
            assistant_text("hi there", StopReason::Stop),
        )
        .await,
        1
    );
    assert_eq!(
        first_token_count(
            vec![
                StreamEvent::ThinkingDelta {
                    content_index: 0,
                    delta: "plan".into(),
                },
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                },
            ],
            AssistantMessage {
                content: vec![ContentBlock::Thinking(ThinkingBlock {
                    thinking: "plan".into(),
                    signature: None,
                    redacted: false,
                })],
                model: "scripted".into(),
                provider: "scripted".into(),
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
            },
        )
        .await,
        1
    );
    assert_eq!(
        first_token_count(
            vec![
                StreamEvent::ToolCallStart { content_index: 0 },
                StreamEvent::ToolCallDelta {
                    content_index: 0,
                    delta: "{}".into(),
                },
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                },
            ],
            assistant_text("tool-start", StopReason::Stop),
        )
        .await,
        1
    );
    assert_eq!(
        first_token_count(
            vec![
                StreamEvent::ToolCallDelta {
                    content_index: 0,
                    delta: "{}".into(),
                },
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                },
            ],
            assistant_text("tool-delta", StopReason::Stop),
        )
        .await,
        1
    );
    assert_eq!(
        first_token_count(
            vec![
                StreamEvent::ToolCallEnd {
                    content_index: 0,
                    tool_call: Arc::new(ToolCall {
                        id: "c1".into(),
                        name: "fast".into(),
                        arguments: json!({}),
                        thought_signature: None,
                    }),
                },
                StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                },
            ],
            assistant_text("tool-end", StopReason::Stop),
        )
        .await,
        1
    );
    assert_eq!(
        first_token_count(Vec::new(), assistant_text("silent", StopReason::Stop)).await,
        0
    );
}

#[tokio::test]
async fn outer_provider_error_closes_request_without_assistant_done() {
    let provider = ScriptedProvider::new(vec![Script::Fail {
        events: Vec::new(),
        error: HarnessError::Transport,
    }]);
    let harness = Harness::new(provider);
    let mut session = Session::new(None);
    let mut traces = TraceBuffer::new();
    let error = run_with_trace(&harness, &mut session, "go", &mut traces)
        .await
        .unwrap_err();
    assert!(matches!(error, HarnessError::Transport));
    assert_eq!(request_starts(traces.events()), vec![(1, 1)]);
    assert!(
        traces
            .events()
            .iter()
            .any(|event| matches!(event, TraceEvent::RequestFailed { step: 1, .. }))
    );
    assert!(
        !traces
            .events()
            .iter()
            .any(|event| matches!(event, TraceEvent::AssistantDone(_)))
    );
    assert!(
        !session
            .transcript()
            .iter()
            .any(|message| matches!(message, Message::Assistant(_)))
    );
}

#[tokio::test]
async fn finalized_error_and_aborted_emit_assistant_done() {
    for (stop_reason, expect) in [
        (StopReason::Error, "failed"),
        (StopReason::Aborted, "aborted"),
    ] {
        let mut message = assistant_text(expect, stop_reason);
        message.error_message = Some(expect.into());
        let provider = ScriptedProvider::new(vec![Script::Complete {
            events: Vec::new(),
            message,
        }]);
        let harness = Harness::new(provider);
        let mut session = Session::new(None);
        let mut traces = TraceBuffer::new();
        let outcome = run_with_trace(&harness, &mut session, "go", &mut traces)
            .await
            .unwrap();
        match stop_reason {
            StopReason::Aborted => assert!(matches!(outcome, RunOutcome::Aborted)),
            _ => assert!(matches!(outcome, RunOutcome::Failed { .. })),
        }
        let done = traces
            .events()
            .iter()
            .find_map(|event| match event {
                TraceEvent::AssistantDone(assistant) => Some(assistant),
                _ => None,
            })
            .expect(expect);
        assert_eq!(done.stop_reason, stop_reason);
        assert_eq!(done.error_message.as_deref(), Some(expect));
        assert!(
            !traces
                .events()
                .iter()
                .any(|event| matches!(event, TraceEvent::RequestFailed { .. }))
        );
        let stored = session
            .transcript()
            .iter()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .expect(expect);
        assert!(Arc::ptr_eq(stored, done));
    }
}

#[tokio::test]
async fn steering_id_is_traced_before_the_next_request() {
    let provider = ScriptedProvider::new(vec![Script::Complete {
        events: Vec::new(),
        message: assistant_text("ok", StopReason::Stop),
    }]);
    let harness = Harness::new(provider);
    let mut session = Session::new(None);
    let id = session.steering().steer_text("follow-up");
    let mut traces = TraceBuffer::new();
    harness
        .continue_run(&mut session, CancelToken::new(), |_| {}, &mut traces)
        .await
        .unwrap();
    let events = traces.events();
    let injected = events
        .iter()
        .position(|event| matches!(event, TraceEvent::SteeringInjected { id: got } if *got == id))
        .expect("steering id");
    let started = events
        .iter()
        .position(|event| {
            matches!(
                event,
                TraceEvent::RequestStart {
                    visible_turn: 1,
                    step: 1,
                    ..
                }
            )
        })
        .expect("request");
    assert!(injected < started);
}

#[tokio::test]
async fn length_reject_and_cancel_still_time_every_started_call() {
    let provider = ScriptedProvider::new(vec![
        Script::Complete {
            events: Vec::new(),
            message: assistant_tools(&[("a", "fast"), ("b", "fast")], StopReason::Length),
        },
        Script::Complete {
            events: Vec::new(),
            message: assistant_text("recovered", StopReason::Stop),
        },
    ]);
    let harness = Harness::new(provider).with_tools(vec![Arc::new(DelayTool::new(
        "fast",
        Duration::from_millis(1),
    ))]);
    let mut session = Session::new(None);
    let mut traces = TraceBuffer::new();
    run_with_trace(&harness, &mut session, "go", &mut traces)
        .await
        .unwrap();
    let mut ids = tool_ids(traces.events());
    ids.sort();
    assert_eq!(ids, vec!["a", "b"]);
}

#[tokio::test]
async fn cancelled_outer_err_is_request_failed() {
    let provider = ScriptedProvider::new(vec![Script::Fail {
        events: Vec::new(),
        error: HarnessError::Cancelled,
    }]);
    let harness = Harness::new(provider);
    let mut session = Session::new(None);
    let mut traces = TraceBuffer::new();
    let error = run_with_trace(&harness, &mut session, "go", &mut traces)
        .await
        .unwrap_err();
    assert!(matches!(error, HarnessError::Cancelled));
    assert!(traces.events().iter().any(|event| matches!(
        event,
        TraceEvent::RequestFailed { error, .. } if error.contains("cancelled")
    )));
}
