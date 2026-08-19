//! The agent loop: two-layer turn cycling with tool dispatch.
//!
//! Inner layer: assistant response → tool batch → next assistant. Before every
//! LLM call, the loop drains the shared steering/follow-up queue into context.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::cancel::CancelToken;
use crate::error::HarnessError;
use crate::events::{AgentEvent, StreamEvent, TraceEvent, TraceSink};
use crate::http::SharedProvider;
use crate::model::{
    AssistantMessage, Message, PromptContext, RequestOptions, StopReason, ToolCall, ToolResult,
    ToolResultPart, ToolSchema, UserMessage,
};
use crate::tools::{
    self, ExecOutcome, ExecutionContext, ExecutionMode, Tool, ToolContext, ToolError,
};
use futures::StreamExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueueMode {
    All,
    OneAtATime,
}

#[derive(Debug)]
pub enum RunOutcome {
    Completed,
    Aborted,
    Failed { error_message: Option<String> },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Budget {
    pub max_messages: Option<u64>,
    pub max_tokens: Option<u64>,
}

/// Cloneable producer for a session's follow-up queue.
#[derive(Clone)]
pub struct SessionSteering {
    queue: Arc<Mutex<SteeringQueue>>,
}

impl SessionSteering {
    pub fn steer(&self, message: impl Into<UserMessage>) -> u64 {
        enqueue_steering(&self.queue, message.into())
    }

    pub fn steer_text(&self, text: impl Into<String>) -> u64 {
        self.steer(UserMessage::text(text))
    }
}

#[derive(Debug)]
struct SteeringQueue {
    next_id: u64,
    messages: VecDeque<QueuedSteering>,
}

#[derive(Debug)]
struct QueuedSteering {
    id: u64,
    message: UserMessage,
}

fn new_steering_queue(next_id: u64) -> Arc<Mutex<SteeringQueue>> {
    Arc::new(Mutex::new(SteeringQueue {
        next_id,
        messages: VecDeque::new(),
    }))
}

fn lock_queue(queue: &Arc<Mutex<SteeringQueue>>) -> std::sync::MutexGuard<'_, SteeringQueue> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn enqueue_steering(queue: &Arc<Mutex<SteeringQueue>>, message: UserMessage) -> u64 {
    let mut queue = lock_queue(queue);
    let id = queue.next_id;
    queue.next_id = queue
        .next_id
        .checked_add(1)
        .expect("steering id space exhausted");
    queue.messages.push_back(QueuedSteering { id, message });
    id
}

/// Compact durable boundary of a completed top-level turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCheckpoint {
    pub message_count: usize,
    pub queue_mode: QueueMode,
    pub budget: Budget,
}

/// A restorable session snapshot. One transcript; compact checkpoint metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionSnapshot {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub queue_mode: QueueMode,
    pub budget: Budget,
    pub checkpoints: Vec<SessionCheckpoint>,
    pub initial_checkpoint: SessionCheckpoint,
}

impl SessionSnapshot {
    pub fn transcript(&self) -> &[Message] {
        &self.messages
    }
}

pub struct Session {
    pub(crate) context: PromptContext,
    queue: Arc<Mutex<SteeringQueue>>,
    queue_mode: QueueMode,
    budget: Budget,
    initial_checkpoint: SessionCheckpoint,
    completed_turns: Vec<SessionCheckpoint>,
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Session {
            context: self.context.clone(),
            queue: new_steering_queue(self.next_steering_id()),
            queue_mode: self.queue_mode,
            budget: self.budget.clone(),
            initial_checkpoint: self.initial_checkpoint.clone(),
            completed_turns: self.completed_turns.clone(),
        }
    }
}

impl Session {
    pub fn new(system_prompt: Option<String>) -> Self {
        Self::with_messages(system_prompt, Vec::new())
    }

    pub fn with_messages(system_prompt: Option<String>, messages: Vec<Message>) -> Self {
        let queue_mode = QueueMode::OneAtATime;
        let budget = Budget::default();
        let initial_checkpoint = SessionCheckpoint {
            message_count: messages.len(),
            queue_mode,
            budget: budget.clone(),
        };
        Session {
            context: PromptContext {
                system_prompt,
                messages,
                tools: Vec::new(),
                provider_model: None,
            },
            queue: new_steering_queue(1),
            queue_mode,
            budget,
            initial_checkpoint,
            completed_turns: Vec::new(),
        }
    }

    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget.clone();
        if self.completed_turns.is_empty() {
            self.initial_checkpoint.budget = budget;
        }
        self
    }

    pub fn set_budget(&mut self, budget: Budget) {
        self.budget = budget;
    }

    pub fn with_queue_mode(mut self, mode: QueueMode) -> Self {
        self.queue_mode = mode;
        if self.completed_turns.is_empty() {
            self.initial_checkpoint.queue_mode = mode;
        }
        self
    }

    pub fn transcript(&self) -> &[Message] {
        &self.context.messages
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.context.system_prompt.as_deref()
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            system_prompt: self.context.system_prompt.clone(),
            messages: self.context.messages.clone(),
            queue_mode: self.queue_mode,
            budget: self.budget.clone(),
            checkpoints: self.completed_turns.clone(),
            initial_checkpoint: self.initial_checkpoint.clone(),
        }
    }

    pub fn with_snapshot(snapshot: SessionSnapshot) -> Result<Self, HarnessError> {
        validate_snapshot(&snapshot)?;
        Ok(Session {
            context: PromptContext {
                system_prompt: snapshot.system_prompt,
                messages: snapshot.messages,
                tools: Vec::new(),
                provider_model: None,
            },
            queue: new_steering_queue(1),
            queue_mode: snapshot.queue_mode,
            budget: snapshot.budget,
            initial_checkpoint: snapshot.initial_checkpoint,
            completed_turns: snapshot.checkpoints,
        })
    }

    /// Appends the prompt to the transcript and returns the snapshot that
    /// durably proves it was admitted. Callers persist this snapshot before
    /// dispatching the first provider request of the turn.
    pub fn admit_prompt(&mut self, prompt: impl Into<UserMessage>) -> SessionSnapshot {
        self.context.messages.push(Message::User(prompt.into()));
        self.snapshot()
    }

    /// Replaces transcript/checkpoint state from a snapshot while keeping the
    /// live steering queue. Used only to roll back an in-memory prompt whose
    /// durable admission write failed.
    pub fn restore_snapshot(&mut self, snapshot: SessionSnapshot) -> Result<(), HarnessError> {
        let restored = Session::with_snapshot(snapshot)?;
        self.context = restored.context;
        self.queue_mode = restored.queue_mode;
        self.budget = restored.budget;
        self.initial_checkpoint = restored.initial_checkpoint;
        self.completed_turns = restored.completed_turns;
        Ok(())
    }

    pub fn with_history(
        system_prompt: Option<String>,
        messages: Vec<Message>,
        completed_turn_boundaries: Vec<usize>,
        queue_mode: QueueMode,
        budget: Budget,
    ) -> Result<Self, HarnessError> {
        let mut previous = 0;
        let mut checkpoints = Vec::with_capacity(completed_turn_boundaries.len());
        for (index, boundary) in completed_turn_boundaries.into_iter().enumerate() {
            if boundary == 0 || boundary <= previous || boundary > messages.len() {
                return Err(HarnessError::InvalidRequest(format!(
                    "completed-turn checkpoint {} has invalid transcript boundary {}; expected a strictly increasing value in 1..={}",
                    index + 1,
                    boundary,
                    messages.len()
                )));
            }
            checkpoints.push(SessionCheckpoint {
                message_count: boundary,
                queue_mode,
                budget: budget.clone(),
            });
            previous = boundary;
        }
        let initial_count = if checkpoints.is_empty() {
            messages.len()
        } else {
            0
        };
        Self::with_snapshot(SessionSnapshot {
            system_prompt,
            messages,
            queue_mode,
            budget: budget.clone(),
            checkpoints,
            initial_checkpoint: SessionCheckpoint {
                message_count: initial_count,
                queue_mode,
                budget,
            },
        })
    }

    pub fn record_turn_checkpoint(&mut self) -> SessionSnapshot {
        let checkpoint = self.current_checkpoint();
        let previous = self
            .completed_turns
            .last()
            .unwrap_or(&self.initial_checkpoint);
        if checkpoint.message_count > previous.message_count {
            self.completed_turns.push(checkpoint);
        }
        self.snapshot()
    }

    pub fn completed_turn_count(&self) -> usize {
        self.completed_turns.len()
    }

    pub fn truncate_completed_turns(&mut self, completed_turns: usize) -> Result<(), HarnessError> {
        let checkpoint = self.checkpoint_for_completed_turns(completed_turns)?;
        self.restore_checkpoint(&checkpoint);
        self.completed_turns.truncate(completed_turns);
        self.clear_queue();
        Ok(())
    }

    pub fn fork_completed_turns(&self, completed_turns: usize) -> Result<Self, HarnessError> {
        let checkpoint = self.checkpoint_for_completed_turns(completed_turns)?;
        let messages = self.context.messages[..checkpoint.message_count].to_vec();
        Session::with_snapshot(SessionSnapshot {
            system_prompt: self.context.system_prompt.clone(),
            messages,
            queue_mode: checkpoint.queue_mode,
            budget: checkpoint.budget.clone(),
            checkpoints: self.completed_turns[..completed_turns].to_vec(),
            initial_checkpoint: self.initial_checkpoint.clone(),
        })
    }

    fn checkpoint_for_completed_turns(
        &self,
        completed_turns: usize,
    ) -> Result<SessionCheckpoint, HarnessError> {
        let available = self.completed_turns.len();
        if completed_turns > available {
            return Err(HarnessError::InvalidRequest(format!(
                "cannot retain {completed_turns} completed turns; session has {available}"
            )));
        }
        Ok(if completed_turns == 0 {
            self.initial_checkpoint.clone()
        } else {
            self.completed_turns[completed_turns - 1].clone()
        })
    }

    fn current_checkpoint(&self) -> SessionCheckpoint {
        SessionCheckpoint {
            message_count: self.context.messages.len(),
            queue_mode: self.queue_mode,
            budget: self.budget.clone(),
        }
    }

    fn restore_checkpoint(&mut self, checkpoint: &SessionCheckpoint) {
        self.context.messages.truncate(checkpoint.message_count);
        self.context.tools.clear();
        self.queue_mode = checkpoint.queue_mode;
        self.budget = checkpoint.budget.clone();
    }

    fn next_steering_id(&self) -> u64 {
        lock_queue(&self.queue).next_id
    }

    pub fn steering(&self) -> SessionSteering {
        SessionSteering {
            queue: Arc::clone(&self.queue),
        }
    }

    pub fn steer(&mut self, message: impl Into<UserMessage>) {
        let _ = enqueue_steering(&self.queue, message.into());
    }

    pub fn steer_text(&mut self, text: impl Into<String>) {
        self.steer(UserMessage::text(text));
    }

    fn take_steering_queue(&self) -> Vec<QueuedSteering> {
        let mut queue = lock_queue(&self.queue);
        match self.queue_mode {
            QueueMode::All => queue.messages.drain(..).collect(),
            QueueMode::OneAtATime => queue.messages.pop_front().into_iter().collect(),
        }
    }

    pub fn queue_len(&self) -> usize {
        lock_queue(&self.queue).messages.len()
    }

    fn clear_queue(&self) {
        lock_queue(&self.queue).messages.clear();
    }

    pub(crate) fn check_budget(&self) -> Result<(), HarnessError> {
        if let Some(max) = self.budget.max_messages {
            let n = self.context.messages.len() as u64;
            if n > max {
                return Err(HarnessError::ContextOverflow {
                    needed: n,
                    budget: max,
                });
            }
        }
        if let Some(max) = self.budget.max_tokens {
            let t = estimate_tokens(&self.context.messages);
            if t > max {
                return Err(HarnessError::ContextOverflow {
                    needed: t,
                    budget: max,
                });
            }
        }
        Ok(())
    }
}

fn validate_snapshot(snapshot: &SessionSnapshot) -> Result<(), HarnessError> {
    let len = snapshot.messages.len();
    if snapshot.initial_checkpoint.message_count > len {
        return Err(HarnessError::InvalidRequest(format!(
            "initial checkpoint count {} exceeds transcript length {len}",
            snapshot.initial_checkpoint.message_count
        )));
    }
    let mut previous = snapshot.initial_checkpoint.message_count;
    for (index, checkpoint) in snapshot.checkpoints.iter().enumerate() {
        if checkpoint.message_count > len {
            return Err(HarnessError::InvalidRequest(format!(
                "completed-turn checkpoint {} count {} exceeds transcript length {len}",
                index + 1,
                checkpoint.message_count
            )));
        }
        if checkpoint.message_count <= previous {
            return Err(HarnessError::InvalidRequest(format!(
                "completed-turn checkpoint {} is not a strict transcript extension of its predecessor",
                index + 1
            )));
        }
        previous = checkpoint.message_count;
    }
    Ok(())
}

pub struct Harness {
    provider: SharedProvider,
    tool_ctx: Arc<ToolContext>,
    tools: Vec<Arc<dyn Tool>>,
    tool_schemas: Vec<ToolSchema>,
    opts: RequestOptions,
    model: Option<String>,
}

impl Harness {
    pub fn new(provider: SharedProvider) -> Self {
        Harness {
            provider,
            tool_ctx: Arc::new(ToolContext::new(".")),
            tools: Vec::new(),
            tool_schemas: Vec::new(),
            opts: RequestOptions::default(),
            model: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.tool_schemas = tools.iter().map(tool_schema).collect();
        self.tools = tools;
        self
    }

    pub fn with_tool_context(mut self, ctx: ToolContext) -> Self {
        self.tool_ctx = Arc::new(ctx);
        self
    }

    pub fn with_request_options(mut self, opts: RequestOptions) -> Self {
        self.opts = opts;
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub async fn run(
        &self,
        session: &mut Session,
        prompt: impl Into<UserMessage>,
        cancel: CancelToken,
        sink: impl FnMut(AgentEvent) + Send,
        trace: impl TraceSink,
    ) -> Result<RunOutcome, HarnessError> {
        session.admit_prompt(prompt);
        self.continue_run(session, cancel, sink, trace).await
    }

    pub async fn run_text(
        &self,
        session: &mut Session,
        prompt: impl Into<String>,
        cancel: CancelToken,
        sink: impl FnMut(AgentEvent) + Send,
        trace: impl TraceSink,
    ) -> Result<RunOutcome, HarnessError> {
        self.run(session, UserMessage::text(prompt), cancel, sink, trace)
            .await
    }

    pub async fn continue_run(
        &self,
        session: &mut Session,
        cancel: CancelToken,
        mut sink: impl FnMut(AgentEvent) + Send,
        mut trace: impl TraceSink,
    ) -> Result<RunOutcome, HarnessError> {
        sink(AgentEvent::RunStarted);
        self.drive(session, cancel, &mut sink, &mut trace).await
    }

    async fn drive(
        &self,
        session: &mut Session,
        cancel: CancelToken,
        sink: &mut (impl FnMut(AgentEvent) + Send),
        trace: &mut impl TraceSink,
    ) -> Result<RunOutcome, HarnessError> {
        session.context.tools.clone_from(&self.tool_schemas);
        emit_prompt_prepared(session, &self.opts, self.model.as_deref(), trace);

        let mut visible_turn = 0usize;
        let mut step = 0usize;
        let mut continue_same_turn = false;

        loop {
            for queued in session.take_steering_queue() {
                session.context.messages.push(Message::User(queued.message));
                sink(AgentEvent::SteeringInjected { id: queued.id });
                trace.emit(TraceEvent::SteeringInjected { id: queued.id });
            }
            session.check_budget()?;
            cancel.check()?;
            sink(AgentEvent::TurnStarted);

            if !continue_same_turn {
                visible_turn += 1;
                step = 0;
            }
            step += 1;

            let assistant = self
                .stream_assistant(session, &cancel, visible_turn, step, sink, trace)
                .await?;
            let stop_reason = assistant.stop_reason;
            let error_message = assistant.error_message.clone();
            let has_tools = assistant.tool_calls().next().is_some();
            session
                .context
                .messages
                .push(Message::Assistant(Arc::clone(&assistant)));
            trace.emit(TraceEvent::AssistantDone(Arc::clone(&assistant)));

            if matches!(stop_reason, StopReason::Error | StopReason::Aborted) {
                let outcome = match stop_reason {
                    StopReason::Aborted => RunOutcome::Aborted,
                    _ => RunOutcome::Failed {
                        error_message: error_message.clone(),
                    },
                };
                sink(AgentEvent::RunEnded {
                    stop_reason,
                    error_message,
                });
                return Ok(outcome);
            }
            sink(AgentEvent::AssistantDone);

            let mut needs_another_turn = false;
            if has_tools {
                let results = if stop_reason == StopReason::Length {
                    reject_all(last_assistant_tool_calls(session), sink, trace)
                } else {
                    self.execute_batch(session, &cancel, sink, trace).await
                };
                let terminate = results.iter().all(|entry| match entry {
                    BatchEntry::Finished { terminate, .. } => *terminate,
                    BatchEntry::Missing => false,
                });
                for entry in results {
                    if let BatchEntry::Finished { result, .. } = entry {
                        session.context.messages.push(Message::ToolResult(result));
                    }
                }
                sink(AgentEvent::TurnFinished);
                needs_another_turn = !terminate;
            } else {
                sink(AgentEvent::TurnFinished);
            }

            continue_same_turn = needs_another_turn;
            if !needs_another_turn && session.queue_len() == 0 {
                sink(AgentEvent::RunEnded {
                    stop_reason,
                    error_message: None,
                });
                return Ok(RunOutcome::Completed);
            }
        }
    }

    async fn stream_assistant(
        &self,
        session: &Session,
        cancel: &CancelToken,
        visible_turn: usize,
        step: usize,
        sink: &mut (impl FnMut(AgentEvent) + Send),
        trace: &mut impl TraceSink,
    ) -> Result<Arc<AssistantMessage>, HarnessError> {
        let started_at = Instant::now();
        trace.emit(TraceEvent::RequestStart {
            visible_turn,
            step,
            started_at,
        });
        let provider = self.provider.clone();
        let ctx = &session.context;
        let opts = &self.opts;
        let model = self.model.as_deref();
        let result = {
            let mut first_token = false;
            let mut stream_sink = |ev: StreamEvent| {
                if !first_token && is_first_token(&ev) {
                    first_token = true;
                    trace.emit(TraceEvent::RequestFirstToken {
                        visible_turn,
                        step,
                        at: Instant::now(),
                    });
                }
                sink(AgentEvent::Assistant(ev));
            };
            provider
                .complete(ctx, opts, model, cancel.clone(), &mut stream_sink)
                .await
        };
        match result {
            Ok(assistant) => Ok(Arc::new(assistant)),
            Err(error) => {
                trace.emit(TraceEvent::RequestFailed {
                    visible_turn,
                    step,
                    failed_at: Instant::now(),
                    error: error.to_string(),
                });
                Err(error)
            }
        }
    }

    async fn execute_batch(
        &self,
        session: &Session,
        cancel: &CancelToken,
        sink: &mut (impl FnMut(AgentEvent) + Send),
        trace: &mut impl TraceSink,
    ) -> Vec<BatchEntry> {
        let calls = last_assistant_tool_calls(session);
        let mut deferred: Vec<(usize, Arc<ToolCall>, Arc<dyn Tool>)> = Vec::new();
        let mut entries: Vec<BatchEntry> = Vec::with_capacity(calls.len());
        for call in &calls {
            sink(AgentEvent::ToolStarted {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
            });
            let started_at = Instant::now();
            let tool = self.tools.iter().find(|t| t.name() == call.name).cloned();
            let immediate: Option<Result<ExecOutcome, ToolError>> = if cancel.is_cancelled() {
                Some(Err(ToolError::Cancelled))
            } else {
                match tool.as_ref() {
                    None => Some(Err(ToolError::Failed(format!(
                        "unknown tool: {}",
                        call.name
                    )))),
                    Some(t) => t.validate(&call.arguments).err().map(Err),
                }
            };
            match immediate {
                Some(res) => {
                    let entry = finished_entry(call, res, ExecutionTiming::closed(started_at));
                    emit_finished_entry(sink, trace, &entry);
                    entries.push(entry);
                }
                None => {
                    if let Some(tool) = tool {
                        deferred.push((entries.len(), Arc::clone(call), tool));
                        entries.push(BatchEntry::Missing);
                    } else {
                        let result = Err(ToolError::Failed(format!("unknown tool: {}", call.name)));
                        let entry =
                            finished_entry(call, result, ExecutionTiming::closed(started_at));
                        emit_finished_entry(sink, trace, &entry);
                        entries.push(entry);
                    }
                }
            }
        }

        let sequential = deferred
            .iter()
            .any(|(_, _, tool)| tool.execution_mode() == ExecutionMode::Sequential);
        if sequential {
            for (slot, call, tool) in deferred {
                let (slot, call, res, timing) =
                    execute_one(self.tool_ctx.clone(), cancel.clone(), slot, call, tool).await;
                let entry = finished_entry(call.as_ref(), res, timing);
                emit_finished_entry(sink, trace, &entry);
                entries[slot] = entry;
            }
        } else {
            let futs = deferred.into_iter().map(|(slot, call, tool)| {
                execute_one(self.tool_ctx.clone(), cancel.clone(), slot, call, tool)
            });
            let mut stream = futures::stream::iter(futs).buffer_unordered(8);
            while let Some((slot, call, res, timing)) = stream.next().await {
                let entry = finished_entry(call.as_ref(), res, timing);
                emit_finished_entry(sink, trace, &entry);
                entries[slot] = entry;
            }
        }
        entries
    }
}

fn last_assistant_tool_calls(session: &Session) -> Vec<Arc<ToolCall>> {
    session
        .context
        .messages
        .last()
        .and_then(Message::as_assistant)
        .map(|assistant| {
            assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    crate::model::ContentBlock::ToolCall(call) => Some(Arc::clone(call)),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn execute_one(
    tool_ctx: Arc<ToolContext>,
    cancel: CancelToken,
    slot: usize,
    call: Arc<ToolCall>,
    tool: Arc<dyn Tool>,
) -> (
    usize,
    Arc<ToolCall>,
    Result<ExecOutcome, ToolError>,
    ExecutionTiming,
) {
    let started_at = Instant::now();
    let exec = ExecutionContext {
        ctx: &tool_ctx,
        cancel,
    };
    let res = tool.execute(call.as_ref(), exec).await;
    (
        slot,
        call,
        res,
        ExecutionTiming {
            started_at,
            finished_at: Instant::now(),
        },
    )
}

fn emit_finished_entry(
    sink: &mut (impl FnMut(AgentEvent) + Send),
    trace: &mut impl TraceSink,
    entry: &BatchEntry,
) {
    let BatchEntry::Finished { result, timing, .. } = entry else {
        unreachable!("only a completed tool entry has a result");
    };
    sink(AgentEvent::ToolFinished {
        result: Arc::clone(result),
    });
    trace.emit(TraceEvent::ToolExecution {
        call_id: result.tool_call_id.clone(),
        started_at: timing.started_at,
        finished_at: timing.finished_at,
        result_preview: result_preview(result),
    });
}

enum BatchEntry {
    Finished {
        result: Arc<ToolResult>,
        terminate: bool,
        timing: ExecutionTiming,
    },
    Missing,
}

#[derive(Clone, Copy)]
struct ExecutionTiming {
    started_at: Instant,
    finished_at: Instant,
}

impl ExecutionTiming {
    fn closed(started_at: Instant) -> Self {
        Self {
            started_at,
            finished_at: Instant::now(),
        }
    }
}

fn finished_entry(
    call: &ToolCall,
    res: Result<ExecOutcome, ToolError>,
    timing: ExecutionTiming,
) -> BatchEntry {
    match res {
        Ok(outcome) => {
            let terminate = outcome.terminate;
            BatchEntry::Finished {
                result: Arc::new(tools::ok_result(call, outcome)),
                terminate,
                timing,
            }
        }
        Err(e) => BatchEntry::Finished {
            result: Arc::new(tools::error_result(call, &e)),
            terminate: false,
            timing,
        },
    }
}

fn tool_schema(t: &Arc<dyn Tool>) -> ToolSchema {
    let spec = t.spec();
    ToolSchema {
        name: spec.name.clone(),
        description: spec.description.clone(),
        parameters: spec.parameters.clone(),
    }
}

fn reject_all(
    calls: Vec<Arc<ToolCall>>,
    sink: &mut (impl FnMut(AgentEvent) + Send),
    trace: &mut impl TraceSink,
) -> Vec<BatchEntry> {
    calls
        .into_iter()
        .map(|call| {
            let started_at = Instant::now();
            sink(AgentEvent::ToolStarted {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
            });
            let result = Arc::new(ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: vec![ToolResultPart::Text(
                    "not executed: the response hit the output token limit, so the arguments may be truncated; re-issue the tool call with complete arguments".into(),
                )],
                is_error: true,
                details: None,
            });
            let entry = BatchEntry::Finished {
                result,
                terminate: false,
                timing: ExecutionTiming::closed(started_at),
            };
            emit_finished_entry(sink, trace, &entry);
            entry
        })
        .collect()
}

fn emit_prompt_prepared(
    session: &Session,
    opts: &RequestOptions,
    model: Option<&str>,
    trace: &mut impl TraceSink,
) {
    let model_hint = model
        .map(str::to_owned)
        .or_else(|| {
            session
                .context
                .provider_model
                .as_ref()
                .map(|target| target.model.clone())
        })
        .unwrap_or_default();
    trace.emit(TraceEvent::PromptPrepared {
        system_prompt: session.context.system_prompt.clone(),
        tools_json: serialize_tools(&session.context.tools),
        options_json: serialize_options(opts),
        model_hint,
    });
}

fn serialize_tools(tools: &[ToolSchema]) -> Arc<str> {
    let payload: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            })
        })
        .collect();
    Arc::from(serde_json::Value::Array(payload).to_string())
}

fn serialize_options(opts: &RequestOptions) -> Arc<str> {
    Arc::from(
        serde_json::json!({
            "max_tokens": opts.max_tokens,
            "temperature": opts.temperature,
            "reasoning": opts.reasoning,
            "service_tier": opts.service_tier.map(wakuwaku_provider::ServiceTier::as_str),
            "omit_sampling": opts.omit_sampling,
            "omit_reasoning_summary": opts.omit_reasoning_summary,
        })
        .to_string(),
    )
}

fn is_first_token(event: &StreamEvent) -> bool {
    match event {
        StreamEvent::TextDelta { delta, .. } | StreamEvent::ThinkingDelta { delta, .. } => {
            !delta.is_empty()
        }
        StreamEvent::ToolCallStart { .. }
        | StreamEvent::ToolCallDelta { .. }
        | StreamEvent::ToolCallEnd { .. } => true,
        _ => false,
    }
}

fn result_preview(result: &ToolResult) -> Arc<str> {
    const LIMIT: usize = 512;
    let mut text = String::new();
    if result.is_error {
        text.push_str("error: ");
    }
    for part in &result.content {
        match part {
            ToolResultPart::Text(part) => {
                if !text.is_empty() && !text.ends_with('\n') && !text.ends_with(' ') {
                    text.push('\n');
                }
                text.push_str(part);
            }
            ToolResultPart::Image { .. } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("[image]");
            }
        }
        if text.len() >= LIMIT {
            break;
        }
    }
    if text.len() > LIMIT {
        let mut end = LIMIT;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Arc::from(text)
}

pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let mut total = 0u64;
    for msg in messages {
        let chars: u64 = match msg {
            Message::User(u) => u.parts.iter().fold(0u64, |total, p| {
                total.saturating_add(match p {
                    crate::model::UserPart::Text(t) => t.len() as u64,
                    crate::model::UserPart::Image { data_b64, .. } => {
                        (data_b64.len() as u64) / 4 * 3 / 4
                    }
                })
            }),
            Message::Assistant(a) => a.content.iter().fold(0u64, |total, b| {
                total.saturating_add(match b {
                    crate::model::ContentBlock::Text(t) => t.text.len() as u64,
                    crate::model::ContentBlock::Thinking(t) => t.thinking.len() as u64,
                    crate::model::ContentBlock::ToolCall(c) => {
                        c.name.len().saturating_add(c.arguments.to_string().len()) as u64
                    }
                })
            }),
            Message::ToolResult(r) => tools::result_size(r),
        };
        total = total.saturating_add(chars.div_ceil(4));
    }
    total
}
