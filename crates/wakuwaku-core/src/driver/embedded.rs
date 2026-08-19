//! In-process HTTP provider driver backed by `wakuwaku-harness`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use futures::channel::oneshot;
use futures::future::{BoxFuture, Either};
use serde_json::Value;
use uuid::Uuid;
use wakuwaku_harness::{AgentEvent, StreamEvent};
use wakuwaku_harness::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, ApprovalTool, CancelToken, EditTool,
    HarnessError, HttpProvider, ListTool, ProviderConfig, Providers, ReadTool, SearchTool,
    SharedProvider, ShellTool, Tool, ToolCall, ToolContext, ToolError, WriteTool,
};
use wakuwaku_harness::{AssistantMessage, ContentBlock, RequestOptions};
use wakuwaku_harness::{Budget, Harness, RunOutcome, Session};
use wakuwaku_protocol::model::{
    ActivityKind, DriverEvent, InteractionMode, PermissionOption, RuntimeMode,
};
use wakuwaku_protocol::{ExternalProvider, ProviderId};

use super::activity;
use super::{DriverEventSender, DriverStartOptions, SessionOptions};

pub(super) struct EmbeddedDriver {
    commands: Sender<Command>,
    active_run: ActiveRunSlot,
    approvals: Arc<ApprovalBridge>,
    events: DriverEventSender,
    steering: Arc<Mutex<Option<wakuwaku_harness::SessionSteering>>>,
    live: Arc<HttpProvider>,
}

type ActiveRunSlot = Arc<Mutex<Option<ActiveRun>>>;

#[derive(Clone)]
struct ActiveRun {
    cancel: CancelToken,
    approvals: Arc<ApprovalBridge>,
}

enum Command {
    Prompt(wakuwaku_harness::UserMessage),
    ApplyOptions {
        options: Box<SessionOptions>,
        response: Sender<bool>,
    },
    Snapshot {
        response: Sender<wakuwaku_harness::SessionSnapshot>,
    },
    Shutdown,
}

/// Coordinates permission requests originating inside a harness tool with
/// synchronous daemon commands. The request is registered before its event is
/// emitted, so an immediately delivered response cannot be lost.
///
/// Waiting uses a futures oneshot so the current-thread runtime can keep
/// polling cancellation and sibling tools.
struct ApprovalBridge {
    events: DriverEventSender,
    state: Mutex<ApprovalState>,
    cancelled: AtomicBool,
}

struct ApprovalState {
    pending: HashMap<String, PendingApproval>,
    session_allow: HashSet<String>,
}

struct PendingApproval {
    response: oneshot::Sender<ApprovalReply>,
    options: Vec<PermissionOption>,
}

enum ApprovalReply {
    AllowOnce,
    AllowSession,
    Deny,
    Cancelled,
}

impl ApprovalBridge {
    fn new(events: DriverEventSender) -> Self {
        Self {
            events,
            state: Mutex::new(ApprovalState {
                pending: HashMap::new(),
                session_allow: HashSet::new(),
            }),
            cancelled: AtomicBool::new(true),
        }
    }

    fn begin_run(&self) {
        self.cancel_pending();
        self.cancelled.store(false, Ordering::Release);
    }

    fn finish_run(&self) {
        self.cancel_pending();
    }

    async fn decide(
        &self,
        request: ApprovalRequest<ToolCall>,
    ) -> Result<ApprovalDecision<()>, ToolError> {
        if request.cancel.is_cancelled() || self.cancelled.load(Ordering::Acquire) {
            return Ok(ApprovalDecision::Cancelled);
        }
        if self.is_session_allowed(&request.value.name) {
            return Ok(ApprovalDecision::Approved(()));
        }

        let request_id = format!("approval-{}", Uuid::new_v4());
        let options = approval_options();
        let (response, receiver) = oneshot::channel();
        {
            let mut state = self.state_lock();
            if request.cancel.is_cancelled() || self.cancelled.load(Ordering::Acquire) {
                return Ok(ApprovalDecision::Cancelled);
            }
            if state.session_allow.contains(&request.value.name) {
                return Ok(ApprovalDecision::Approved(()));
            }
            state.pending.insert(
                request_id.clone(),
                PendingApproval {
                    response,
                    options: options.clone(),
                },
            );
        }

        if !self.events.send(DriverEvent::Permission {
            request_id: request_id.clone(),
            title: format!("Allow {}?", request.value.name),
            detail: approval_detail(&request.value),
            options,
        }) {
            self.state_lock().pending.remove(&request_id);
            return Ok(ApprovalDecision::Cancelled);
        }

        match futures::future::select(receiver, request.cancel.cancelled()).await {
            Either::Left((Ok(ApprovalReply::AllowOnce), _)) => Ok(ApprovalDecision::Approved(())),
            Either::Left((Ok(ApprovalReply::AllowSession), _)) => {
                self.state_lock()
                    .session_allow
                    .insert(request.value.name.clone());
                Ok(ApprovalDecision::Approved(()))
            }
            Either::Left((Ok(ApprovalReply::Deny), _)) => Ok(ApprovalDecision::Denied),
            Either::Left((Ok(ApprovalReply::Cancelled), _))
            | Either::Left((Err(_), _))
            | Either::Right(_) => {
                self.state_lock().pending.remove(&request_id);
                Ok(ApprovalDecision::Cancelled)
            }
        }
    }

    fn respond(&self, request_id: &str, option_id: &str) -> anyhow::Result<()> {
        let (response, reply) = {
            let mut state = self.state_lock();
            let reply = {
                let waiting = state
                    .pending
                    .get(request_id)
                    .ok_or_else(|| anyhow!("permission request {request_id} is not pending"))?;
                let option = waiting.options.iter().find(|option| option.id == option_id);
                let option = option.ok_or_else(|| {
                    anyhow!("permission option {option_id:?} is not valid for request {request_id}")
                })?;
                approval_reply(&option.id)
            };
            let waiting = state
                .pending
                .remove(request_id)
                .ok_or_else(|| anyhow!("permission request {request_id} is not pending"))?;
            (waiting.response, reply)
        };

        response
            .send(reply)
            .map_err(|_| anyhow!("permission request {request_id} is no longer waiting"))
    }

    fn cancel_pending(&self) {
        self.cancelled.store(true, Ordering::Release);
        let pending = std::mem::take(&mut self.state_lock().pending);
        for (_, pending) in pending {
            let _ = pending.response.send(ApprovalReply::Cancelled);
        }
    }

    fn is_session_allowed(&self, tool: &str) -> bool {
        self.state_lock().session_allow.contains(tool)
    }

    fn state_lock(&self) -> std::sync::MutexGuard<'_, ApprovalState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ApprovalGate<ToolCall> for ApprovalBridge {
    type Approved = ();

    fn approve<'a>(
        &'a self,
        request: ApprovalRequest<ToolCall>,
    ) -> BoxFuture<'a, Result<ApprovalDecision<()>, ToolError>> {
        Box::pin(async move { self.decide(request).await })
    }
}

#[derive(Clone)]
struct ConfigInput<'a> {
    limits: wakuwaku_protocol::ProviderLimits,
    mode: RuntimeMode,
    interaction_mode: InteractionMode,
    model: Option<&'a str>,
    reasoning_effort: Option<&'a str>,
    service_tier: Option<wakuwaku_protocol::ServiceTier>,
    context_window: Option<&'a str>,
    capabilities: wakuwaku_protocol::ModelCapabilities,
}

struct RuntimeConfig {
    mode: RuntimeMode,
    interaction_mode: InteractionMode,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<wakuwaku_protocol::ServiceTier>,
    context_window: u64,
    capabilities: wakuwaku_protocol::ModelCapabilities,
    limits: wakuwaku_protocol::ProviderLimits,
}

impl RuntimeConfig {
    fn from_start(options: &DriverStartOptions) -> anyhow::Result<Self> {
        Self::from_values(ConfigInput {
            limits: options.limits,
            mode: options.mode,
            interaction_mode: options.interaction_mode,
            model: options.model.as_deref(),
            reasoning_effort: options.reasoning_effort.as_deref(),
            service_tier: options.service_tier,
            context_window: options.context_window.as_deref(),
            capabilities: options.capabilities,
        })
    }

    fn with_options(provider: &ExternalProvider, options: &SessionOptions) -> anyhow::Result<Self> {
        let _ = provider;
        let reconfigure = options
            .reconfigure
            .as_ref()
            .ok_or_else(|| anyhow!("session apply is missing model capabilities"))?;
        Self::from_values(ConfigInput {
            limits: reconfigure.limits,
            mode: options.mode,
            interaction_mode: options.interaction_mode,
            model: options.model.as_deref(),
            reasoning_effort: options.reasoning_effort.as_deref(),
            service_tier: options.service_tier,
            context_window: options.context_window.as_deref(),
            capabilities: reconfigure.capabilities,
        })
    }

    fn from_values(input: ConfigInput<'_>) -> anyhow::Result<Self> {
        let ConfigInput {
            limits,
            mode,
            interaction_mode,
            model,
            reasoning_effort,
            service_tier,
            context_window,
            capabilities,
        } = input;
        let model = match model {
            Some(model) if model.trim().is_empty() => bail!("model must not be empty"),
            Some(model) => model.trim().to_owned(),
            // Endpoint config carries no default model: the picker only offers
            // catalog models, so a session always names one explicitly.
            None => bail!("a catalog model must be selected for this provider"),
        };
        if let Some(tier) = service_tier
            && !capabilities.service_tier
        {
            bail!("service tier {tier} is not supported by this model");
        }
        let context_window = selected_context_window(limits, context_window)?;
        if limits.max_output_tokens > context_window {
            bail!(
                "selected context window {context_window} is smaller than the provider output limit {}",
                limits.max_output_tokens
            );
        }

        Ok(Self {
            mode,
            interaction_mode,
            model,
            reasoning_effort: option_text(reasoning_effort).map(str::to_owned),
            service_tier: service_tier.filter(|_| capabilities.service_tier),
            context_window,
            capabilities,
            limits,
        })
    }
}

struct HarnessFactory {
    provider: ExternalProvider,
    http: SharedProvider,
    live: Arc<HttpProvider>,
    cwd: std::path::PathBuf,
    approvals: Arc<ApprovalBridge>,
}

impl HarnessFactory {
    fn build(&self, config: &RuntimeConfig) -> anyhow::Result<Harness> {
        let policy = tool_policy(config.mode, config.interaction_mode);
        let mut tool_context = ToolContext::new(self.cwd.clone());
        tool_context.allow_shell = policy != ToolPolicy::ReadOnly;

        Ok(Harness::new(Arc::clone(&self.http))
            .with_tool_context(tool_context)
            .with_tools(build_tools(policy, &self.approvals))
            .with_model(config.model.clone())
            .with_request_options(request_options(config)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolPolicy {
    ReadOnly,
    AskEveryMutation,
    AskBeforeShell,
    FullAccess,
}

fn tool_policy(mode: RuntimeMode, interaction_mode: InteractionMode) -> ToolPolicy {
    if matches!(interaction_mode, InteractionMode::Plan) {
        return ToolPolicy::ReadOnly;
    }
    match mode {
        RuntimeMode::Ask => ToolPolicy::AskEveryMutation,
        RuntimeMode::AutoAcceptEdits => ToolPolicy::AskBeforeShell,
        RuntimeMode::FullAccess => ToolPolicy::FullAccess,
    }
}

fn approval_required(policy: ToolPolicy, name: &str) -> bool {
    matches!(
        (policy, name),
        (ToolPolicy::AskEveryMutation, "edit" | "write" | "shell")
            | (ToolPolicy::AskBeforeShell, "shell")
    )
}

fn gated_tool<T: Tool + 'static>(
    tool: T,
    policy: ToolPolicy,
    approvals: &Arc<ApprovalBridge>,
) -> Arc<dyn Tool> {
    if approval_required(policy, tool.name()) {
        Arc::new(ApprovalTool::new(tool, Arc::clone(approvals)))
    } else {
        Arc::new(tool)
    }
}

fn build_tools(policy: ToolPolicy, approvals: &Arc<ApprovalBridge>) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::unbound()),
        Arc::new(ListTool::unbound()),
        Arc::new(SearchTool::unbound()),
    ];
    if policy != ToolPolicy::ReadOnly {
        tools.push(gated_tool(EditTool::unbound(), policy, approvals));
        tools.push(gated_tool(WriteTool::unbound(), policy, approvals));
        tools.push(gated_tool(ShellTool::unbound(), policy, approvals));
    }
    tools
}

fn request_options(config: &RuntimeConfig) -> RequestOptions {
    RequestOptions {
        max_tokens: Some(config.limits.max_output_tokens.min(config.context_window)),
        temperature: None,
        reasoning: config
            .capabilities
            .reasoning_effort
            .then(|| config.reasoning_effort.clone())
            .flatten(),
        service_tier: config
            .service_tier
            .filter(|_| config.capabilities.service_tier),
        omit_sampling: !config.capabilities.sampling,
        omit_reasoning_summary: !config.capabilities.reasoning_summary,
    }
}

fn budget(config: &RuntimeConfig) -> Budget {
    Budget {
        max_messages: None,
        // A context window contains both prompt and generated tokens. Reserving
        // the requested output limit prevents the driver from knowingly sending
        // a prompt which leaves no room for the response.
        max_tokens: Some(config.context_window - config.limits.max_output_tokens),
    }
}

fn selected_context_window(
    limits: wakuwaku_protocol::ProviderLimits,
    selected: Option<&str>,
) -> anyhow::Result<u64> {
    let Some(selected) = option_text(selected) else {
        return Ok(limits.context_window);
    };
    let context_window = selected
        .parse::<u64>()
        .with_context(|| format!("context window {selected:?} must be a positive token count"))?;
    if context_window == 0 {
        bail!("context window must be a positive token count");
    }
    if context_window > limits.context_window {
        bail!(
            "selected context window {context_window} exceeds provider limit {}",
            limits.context_window
        );
    }
    Ok(context_window)
}

fn option_text(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

impl EmbeddedDriver {
    pub(super) fn start(
        provider_id: ProviderId,
        options: DriverStartOptions,
        events: DriverEventSender,
        session_id: Uuid,
        handoff: Arc<crate::trajectory::TraceHandoff>,
    ) -> anyhow::Result<Self> {
        if options.provider.id != provider_id {
            bail!("provider configuration does not match the selected endpoint");
        }

        let config = RuntimeConfig::from_start(&options)?;
        let provider = build_provider(&options)?;
        let providers = Providers::new();
        providers
            .set_providers(vec![provider])
            .map_err(|error| anyhow!(error.to_string()))?;
        let live = Arc::new(
            HttpProvider::new(providers, provider_id.as_str())
                .context("could not construct embedded HTTP provider")?,
        );
        let http: SharedProvider = Arc::clone(&live) as SharedProvider;

        let approvals = Arc::new(ApprovalBridge::new(events.clone()));
        let factory = HarnessFactory {
            provider: options.provider.clone(),
            http,
            live: Arc::clone(&live),
            cwd: options.cwd.clone(),
            approvals: Arc::clone(&approvals),
        };
        let harness = factory.build(&config)?;
        let session = Session::with_snapshot(options.snapshot)
            .map_err(|error| anyhow!(error.to_string()))?
            .with_budget(budget(&config));
        let session_steering = session.steering();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create embedded provider runtime")?;

        let (commands, receiver) = unbounded();
        let active_run = Arc::new(Mutex::new(None));
        let worker_active_run = Arc::clone(&active_run);
        let worker_events = events.clone();
        thread::Builder::new()
            .name(format!("wakuwaku-embedded-{}", provider_id.as_str()))
            .spawn(move || {
                worker(Worker {
                    runtime,
                    receiver,
                    active_run: worker_active_run,
                    factory,
                    harness,
                    session,
                    config,
                    events: worker_events,
                    session_id,
                    handoff,
                })
            })
            .context("failed to start embedded provider driver")?;

        Ok(Self {
            commands,
            active_run,
            approvals,
            events,
            steering: Arc::new(Mutex::new(Some(session_steering))),
            live,
        })
    }

    fn send_command(&self, command: Command, action: &str) {
        if self.commands.send(command).is_err() {
            let _ = self.events.send(DriverEvent::Error(format!(
                "embedded provider runtime exited before it could {action}"
            )));
        }
    }

    fn is_active(&self) -> bool {
        self.active_run
            .lock()
            .map(|active| active.is_some())
            .unwrap_or(true)
    }
}

impl EmbeddedDriver {
    pub(super) fn prompt(&self, prompt: wakuwaku_harness::UserMessage) {
        if self.is_active() {
            self.steer(prompt);
            return;
        }
        self.send_command(Command::Prompt(prompt), "accept a prompt");
    }

    pub(super) fn supports_steer(&self) -> bool {
        true
    }

    pub(super) fn steer(&self, prompt: wakuwaku_harness::UserMessage) {
        let text = wakuwaku_harness::UserMessage::text_of(&prompt.parts);
        let handle = self
            .steering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match handle {
            Some(handle) => {
                handle.steer(prompt);
                let _ = self
                    .events
                    .send(DriverEvent::SteerAccepted { message: text });
            }
            None => {
                let _ = self.events.send(DriverEvent::SteerRejected {
                    message: text,
                    reason: "the embedded HTTP runtime is not accepting steering".into(),
                });
            }
        }
    }

    pub(super) fn cancel(&self) {
        let active = self
            .active_run
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(active) = active {
            active.cancel.cancel();
            active.approvals.cancel_pending();
        }
    }

    pub(super) fn respond(&self, request_id: String, option_id: String) {
        if let Err(error) = self.approvals.respond(&request_id, &option_id) {
            let _ = self.events.send(DriverEvent::Error(error.to_string()));
        }
    }

    pub(super) fn respond_user_input(
        &self,
        request_id: String,
        _answers: Vec<wakuwaku_protocol::model::UserInputAnswer>,
    ) -> anyhow::Result<()> {
        bail!("no pending user input request {request_id}")
    }

    pub(super) fn reject_background_stop(&self, key: wakuwaku_protocol::model::BackgroundWorkKey) {
        let _ = self.events.send(DriverEvent::BackgroundWork(Box::new(
            wakuwaku_protocol::model::BackgroundWorkEvent::StopFailed {
                key,
                message: "this embedded runtime cannot stop detached work".into(),
            },
        )));
    }

    pub(super) fn replace_auth(
        &self,
        auth: wakuwaku_harness::Auth,
        extra_auth_headers: Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        self.live
            .replace_auth(auth, extra_auth_headers)
            .map_err(|error| anyhow!(error.to_string()))
    }

    pub(super) fn apply_options(&self, options: SessionOptions) -> bool {
        if self.is_active() {
            return false;
        }

        let (response, receiver) = bounded(1);
        if self
            .commands
            .send(Command::ApplyOptions {
                options: Box::new(options),
                response,
            })
            .is_err()
        {
            return false;
        }
        receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or(false)
    }

    pub(super) fn snapshot(&self) -> anyhow::Result<wakuwaku_harness::SessionSnapshot> {
        let (response, receiver) = bounded(1);
        self.commands
            .send(Command::Snapshot { response })
            .map_err(|_| anyhow!("embedded provider runtime is not running"))?;
        receiver
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| anyhow!("embedded provider runtime did not return a snapshot"))
    }
}

impl Drop for EmbeddedDriver {
    fn drop(&mut self) {
        self.cancel();
        let _ = self.commands.send(Command::Shutdown);
    }
}

fn build_provider(options: &DriverStartOptions) -> anyhow::Result<ProviderConfig> {
    Ok(ProviderConfig {
        endpoint: options.provider.clone(),
        limits: options.limits,
        auth: options.auth.clone(),
        transport: options.transport,
        extra_auth_headers: options.extra_auth_headers.clone(),
    })
}

struct Worker {
    runtime: tokio::runtime::Runtime,
    receiver: Receiver<Command>,
    active_run: ActiveRunSlot,
    factory: HarnessFactory,
    harness: Harness,
    session: Session,
    config: RuntimeConfig,
    events: DriverEventSender,
    session_id: Uuid,
    handoff: Arc<crate::trajectory::TraceHandoff>,
}

fn worker(mut worker: Worker) {
    let mut event_bridge = EventBridge::new(
        worker.events.clone(),
        worker.factory.provider.id.clone(),
        worker.config.model.clone(),
        worker.config.context_window,
    );
    let _ = worker.events.send(DriverEvent::Connected);

    while let Ok(command) = worker.receiver.recv() {
        match command {
            Command::Prompt(prompt) => run_prompt(&mut worker, &mut event_bridge, prompt),
            Command::ApplyOptions { options, response } => {
                let result = apply_options(
                    &mut worker.factory,
                    &mut worker.harness,
                    &mut worker.session,
                    &mut worker.config,
                    &mut event_bridge,
                    *options,
                );
                if let Err(error) = &result {
                    let _ = worker.events.send(DriverEvent::Error(error.to_string()));
                }
                let _ = response.send(result.is_ok());
            }
            Command::Snapshot { response } => {
                let _ = response.send(worker.session.snapshot());
            }
            Command::Shutdown => break,
        }
    }

    worker.factory.approvals.finish_run();
    let _ = worker.events.send(DriverEvent::ProcessExited);
}

fn apply_options(
    factory: &mut HarnessFactory,
    harness: &mut Harness,
    session: &mut Session,
    config: &mut RuntimeConfig,
    event_bridge: &mut EventBridge,
    options: SessionOptions,
) -> anyhow::Result<()> {
    if let Some(reconfigure) = &options.reconfigure {
        factory.provider = reconfigure.provider.clone();
        factory
            .live
            .replace_config(ProviderConfig {
                endpoint: reconfigure.provider.clone(),
                limits: reconfigure.limits,
                auth: reconfigure.auth.clone(),
                transport: reconfigure.transport,
                extra_auth_headers: reconfigure.extra_auth_headers.clone(),
            })
            .map_err(|error| anyhow!(error.to_string()))?;
    }
    let next_config = RuntimeConfig::with_options(&factory.provider, &options)?;
    let next_harness = factory.build(&next_config)?;
    session.set_budget(budget(&next_config));

    *harness = next_harness;
    *config = next_config;
    event_bridge.provider = factory.provider.id.clone();
    event_bridge.model = config.model.clone();
    event_bridge.context_window = config.context_window;
    Ok(())
}

fn run_prompt(
    worker: &mut Worker,
    event_bridge: &mut EventBridge,
    prompt: wakuwaku_harness::UserMessage,
) {
    worker.factory.approvals.begin_run();
    let cancel = CancelToken::new();
    {
        let mut active = worker
            .active_run
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = Some(ActiveRun {
            cancel: cancel.clone(),
            approvals: Arc::clone(&worker.factory.approvals),
        });
    }

    let _ = worker.events.send(DriverEvent::TurnStarted);
    let mut sink = |event| event_bridge.emit(event);
    let mut traces = worker.handoff.sink(worker.session_id);
    let result = worker.runtime.block_on(worker.harness.run(
        &mut worker.session,
        prompt,
        cancel,
        &mut sink,
        &mut traces,
    ));

    {
        let mut active = worker
            .active_run
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = None;
    }
    worker.factory.approvals.finish_run();
    worker.session.record_turn_checkpoint();
    emit_run_result(&worker.events, &worker.session, result);
}

fn emit_run_result(
    events: &DriverEventSender,
    session: &Session,
    result: Result<RunOutcome, HarnessError>,
) {
    match result {
        Ok(outcome) => emit_outcome(events, outcome, session),
        Err(error) => {
            let cancelled = matches!(error, HarnessError::Cancelled);
            let summary = (!cancelled).then(|| error.to_string());
            if let Some(message) = summary.as_ref() {
                let _ = events.send(DriverEvent::Error(message.clone()));
            }
            let _ = events.send(DriverEvent::TurnFinished {
                success: false,
                summary,
            });
        }
    }
}

fn emit_outcome(events: &DriverEventSender, outcome: RunOutcome, session: &Session) {
    let (success, failure) = match &outcome {
        RunOutcome::Completed => (true, None),
        RunOutcome::Aborted => (false, None),
        RunOutcome::Failed { error_message } => {
            let failure = error_message
                .as_deref()
                .filter(|error| !error.is_empty())
                .map(str::to_owned);
            if let Some(error) = failure.as_ref() {
                let _ = events.send(DriverEvent::Error(error.clone()));
            }
            (false, failure)
        }
    };
    let summary = session
        .transcript()
        .last()
        .and_then(|message| message.as_assistant().and_then(assistant_text))
        .or(failure);
    let _ = events.send(DriverEvent::TurnFinished { success, summary });
}

#[derive(Clone)]
struct ToolActivity {
    kind: ActivityKind,
    title: String,
    arguments: Option<Value>,
}

struct EventBridge {
    events: DriverEventSender,
    provider: ProviderId,
    model: String,
    context_window: u64,
    tools: HashMap<String, ToolActivity>,
}

impl EventBridge {
    fn new(
        events: DriverEventSender,
        provider: ProviderId,
        model: String,
        context_window: u64,
    ) -> Self {
        Self {
            events,
            provider,
            model,
            context_window,
            tools: HashMap::new(),
        }
    }

    fn emit(&mut self, event: AgentEvent) {
        match event {
            // TraceEvent uses the separate handoff sink and never enters EventBridge.
            // Lifecycle AgentEvents are dropped so one user prompt stays one
            // Chat turn on the wire.
            AgentEvent::RunStarted
            | AgentEvent::TurnStarted
            | AgentEvent::AssistantDone
            | AgentEvent::TurnFinished
            | AgentEvent::RunEnded { .. } => {}
            AgentEvent::Assistant(stream) => self.emit_stream(stream),
            AgentEvent::ToolStarted {
                tool_call_id,
                tool_name,
            } => self.tool_started(tool_call_id, tool_name),
            AgentEvent::SteeringInjected { .. } => {}
            AgentEvent::ToolFinished { result } => self.tool_finished(result),
        }
    }

    fn emit_stream(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Start
            | StreamEvent::TextStart { .. }
            | StreamEvent::TextEnd { .. }
            | StreamEvent::ThinkingStart { .. }
            | StreamEvent::ThinkingEnd { .. }
            | StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolCallDelta { .. } => {}
            StreamEvent::TextDelta { delta, .. } => {
                let _ = self.events.send(DriverEvent::TextDelta(delta));
            }
            StreamEvent::ThinkingDelta { delta, .. } => {
                let _ = self.events.send(DriverEvent::ReasoningDelta(delta));
            }
            StreamEvent::ToolCallEnd { tool_call, .. } => self.tool_call(tool_call),
            StreamEvent::Done { usage, .. } | StreamEvent::Failed { usage, .. } => {
                if usage.input == 0
                    && usage.output == 0
                    && usage.cache_read == 0
                    && usage.cache_write == 0
                {
                    return;
                }
                let _ = self.events.send(DriverEvent::UsageUpdated {
                    event_id: Uuid::new_v4(),
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    timestamp_ms: unix_time_ms(),
                    input: usage.input,
                    output: usage.output,
                    cache_read: usage.cache_read,
                    cache_write: usage.cache_write,
                    reasoning: usage.reasoning,
                    context_tokens: Some(usage.total_tokens),
                    context_window: Some(self.context_window),
                });
            }
        }
    }

    fn tool_call(&mut self, call: std::sync::Arc<wakuwaku_harness::ToolCall>) {
        let id = call.id.clone();
        let activity = ToolActivity {
            kind: ActivityKind::from_tool_name(&call.name),
            title: call.name.clone(),
            arguments: Some(call.arguments.clone()),
        };
        self.emit_tool(&id, &activity, false, false);
        self.tools.insert(id, activity);
    }

    fn tool_started(&mut self, id: String, name: String) {
        let activity = self
            .tools
            .entry(id.clone())
            .or_insert_with(|| ToolActivity {
                kind: ActivityKind::from_tool_name(&name),
                title: name.clone(),
                arguments: None,
            });
        if activity.title != name {
            activity.title = name;
            activity.kind = ActivityKind::from_tool_name(&activity.title);
        }
        let activity = activity.clone();
        self.emit_tool(&id, &activity, false, false);
    }

    fn tool_finished(&mut self, result: std::sync::Arc<wakuwaku_harness::ToolResult>) {
        let id = result.tool_call_id.clone();
        let mut activity = self.tools.remove(&id).unwrap_or(ToolActivity {
            kind: ActivityKind::from_tool_name(&result.tool_name),
            title: result.tool_name.clone(),
            arguments: None,
        });
        if activity.title != result.tool_name {
            activity.title = result.tool_name.clone();
            activity.kind = ActivityKind::from_tool_name(&activity.title);
        }
        let text_output = result
            .details
            .is_none()
            .then(|| {
                let text = result
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        wakuwaku_harness::ToolResultPart::Text(text) => Some(text.as_str()),
                        wakuwaku_harness::ToolResultPart::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.is_empty()).then_some(Value::String(text))
            })
            .flatten();
        let output = result.details.as_deref().or(text_output.as_ref());
        let activity_item = activity::tool_activity(
            Some(id.clone()),
            activity.kind,
            activity.title,
            activity::ToolActivityView {
                arguments: activity.arguments.as_ref(),
                output,
                image_source: None,
                failed: result.is_error,
                complete: true,
            },
        );
        let _ = self
            .events
            .send(DriverEvent::RichActivity(Box::new(activity_item)));
    }

    fn emit_tool(&self, id: &str, activity_state: &ToolActivity, failed: bool, complete: bool) {
        let activity = activity::tool_activity(
            Some(id.to_owned()),
            activity_state.kind,
            activity_state.title.clone(),
            activity::ToolActivityView {
                arguments: activity_state.arguments.as_ref(),
                output: None,
                image_source: None,
                failed,
                complete,
            },
        );
        let _ = self
            .events
            .send(DriverEvent::RichActivity(Box::new(activity)));
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn approval_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            id: "allow".into(),
            label: "Allow once".into(),
            allow: true,
        },
        PermissionOption {
            id: "allow-session".into(),
            label: "Allow this tool for the session".into(),
            allow: true,
        },
        PermissionOption {
            id: "deny".into(),
            label: "Deny".into(),
            allow: false,
        },
    ]
}

fn approval_reply(id: &str) -> ApprovalReply {
    match id {
        "allow" => ApprovalReply::AllowOnce,
        "allow-session" => ApprovalReply::AllowSession,
        _ => ApprovalReply::Deny,
    }
}

fn approval_detail(call: &ToolCall) -> String {
    let arguments = serde_json::to_string_pretty(&call.arguments)
        .unwrap_or_else(|_| call.arguments.to_string());
    let arguments = truncate(&arguments, 4_000);
    format!("{} requested with:\n{arguments}", call.name)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("\n…(truncated)");
    truncated
}

fn assistant_text(message: &AssistantMessage) -> Option<String> {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::test_event_channel;
    use wakuwaku_harness::ApprovalRequest;

    #[test]
    fn failed_run_surfaces_provider_error_as_turn_summary() {
        let (events, rx) = test_event_channel();
        let session = Session::new(None);
        emit_outcome(
            &events,
            RunOutcome::Failed {
                error_message: Some("connection reset by peer".into()),
            },
            &session,
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            DriverEvent::Error(message) if message.contains("connection reset")
        ));
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            DriverEvent::TurnFinished {
                success: false,
                summary: Some(summary),
            } => assert!(summary.contains("connection reset"), "{summary}"),
            other => panic!("unexpected event {other:?}"),
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        }
    }

    fn permission_id(rx: &crossbeam_channel::Receiver<DriverEvent>) -> String {
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            DriverEvent::Permission { request_id, .. } => request_id,
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn ask_policy_can_allow_once_deny_and_session_allow() {
        assert!(approval_required(ToolPolicy::AskEveryMutation, "shell"));
        assert!(approval_required(ToolPolicy::AskEveryMutation, "write"));
        assert!(!approval_required(ToolPolicy::AskEveryMutation, "read"));
        assert!(approval_required(ToolPolicy::AskBeforeShell, "shell"));
        assert!(!approval_required(ToolPolicy::AskBeforeShell, "write"));
        assert!(!approval_required(ToolPolicy::FullAccess, "shell"));
        assert!(!approval_required(ToolPolicy::ReadOnly, "read"));
        assert_eq!(
            tool_policy(RuntimeMode::Ask, InteractionMode::Plan),
            ToolPolicy::ReadOnly
        );

        let (events, rx) = test_event_channel();
        let bridge = Arc::new(ApprovalBridge::new(events));
        bridge.begin_run();
        let cancel = CancelToken::new();
        let worker = {
            let bridge = Arc::clone(&bridge);
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                futures::executor::block_on(bridge.decide(ApprovalRequest {
                    value: call("shell"),
                    cancel,
                }))
            })
        };
        let request_id = permission_id(&rx);
        bridge.respond(&request_id, "allow").unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Ok(ApprovalDecision::Approved(()))
        ));

        let (events, rx) = test_event_channel();
        let bridge = Arc::new(ApprovalBridge::new(events));
        bridge.begin_run();
        let cancel = CancelToken::new();
        let worker = {
            let bridge = Arc::clone(&bridge);
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                futures::executor::block_on(bridge.decide(ApprovalRequest {
                    value: call("write"),
                    cancel,
                }))
            })
        };
        let request_id = permission_id(&rx);
        bridge.respond(&request_id, "deny").unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Ok(ApprovalDecision::Denied)
        ));

        let (events, rx) = test_event_channel();
        let bridge = Arc::new(ApprovalBridge::new(events));
        bridge.begin_run();
        let cancel = CancelToken::new();
        let worker = {
            let bridge = Arc::clone(&bridge);
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                futures::executor::block_on(bridge.decide(ApprovalRequest {
                    value: call("edit"),
                    cancel,
                }))
            })
        };
        let request_id = permission_id(&rx);
        bridge.respond(&request_id, "allow-session").unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Ok(ApprovalDecision::Approved(()))
        ));
        let second = futures::executor::block_on(bridge.decide(ApprovalRequest {
            value: call("edit"),
            cancel: CancelToken::new(),
        }));
        assert!(matches!(second, Ok(ApprovalDecision::Approved(()))));
        assert!(rx.try_recv().is_err());
    }

    fn billed_usage() -> wakuwaku_harness::Usage {
        wakuwaku_harness::Usage {
            input: 4,
            output: 2,
            cache_read: 1,
            cache_write: 0,
            reasoning: Some(3),
            total_tokens: 7,
        }
    }

    fn take_usage(rx: &crossbeam_channel::Receiver<DriverEvent>) -> DriverEvent {
        rx.recv_timeout(Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn done_and_failed_with_tokens_each_emit_one_billed_event() {
        let (events, rx) = test_event_channel();
        let mut bridge = EventBridge::new(
            events,
            ProviderId::new("openai-responses"),
            "gpt-5.3".into(),
            128_000,
        );
        bridge.emit_stream(StreamEvent::Done {
            usage: billed_usage(),
            stop_reason: wakuwaku_harness::StopReason::Stop,
        });
        bridge.emit_stream(StreamEvent::Failed {
            usage: billed_usage(),
            stop_reason: wakuwaku_harness::StopReason::Error,
            error_message: Some("boom".into()),
        });
        match take_usage(&rx) {
            DriverEvent::UsageUpdated {
                provider,
                input,
                output,
                cache_read,
                ..
            } => {
                assert_eq!(provider.as_str(), "openai-responses");
                assert_eq!(input, 4);
                assert_eq!(output, 2);
                assert_eq!(cache_read, 1);
            }
            other => panic!("{other:?}"),
        }
        match take_usage(&rx) {
            DriverEvent::UsageUpdated { input, .. } => assert_eq!(input, 4),
            other => panic!("{other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn zero_token_responses_do_not_emit_usage() {
        let (events, rx) = test_event_channel();
        let mut bridge = EventBridge::new(
            events,
            ProviderId::new("anthropic"),
            "claude-fable-5".into(),
            200_000,
        );
        bridge.emit_stream(StreamEvent::Done {
            usage: wakuwaku_harness::Usage::default(),
            stop_reason: wakuwaku_harness::StopReason::Stop,
        });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn large_untrusted_token_fields_still_emit_without_summing() {
        let (events, rx) = test_event_channel();
        let mut bridge = EventBridge::new(
            events,
            ProviderId::new("openai-responses"),
            "gpt-5.3".into(),
            128_000,
        );
        bridge.emit_stream(StreamEvent::Done {
            usage: wakuwaku_harness::Usage {
                input: u64::MAX,
                output: u64::MAX,
                cache_read: u64::MAX,
                cache_write: u64::MAX,
                reasoning: None,
                total_tokens: 0,
            },
            stop_reason: wakuwaku_harness::StopReason::Stop,
        });
        match take_usage(&rx) {
            DriverEvent::UsageUpdated {
                input,
                output,
                cache_read,
                cache_write,
                ..
            } => {
                assert_eq!(input, u64::MAX);
                assert_eq!(output, u64::MAX);
                assert_eq!(cache_read, u64::MAX);
                assert_eq!(cache_write, u64::MAX);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_loop_done_then_final_done_bills_twice() {
        let (events, rx) = test_event_channel();
        let mut bridge = EventBridge::new(
            events,
            ProviderId::new("anthropic"),
            "claude-fable-5".into(),
            200_000,
        );
        bridge.emit_stream(StreamEvent::Done {
            usage: billed_usage(),
            stop_reason: wakuwaku_harness::StopReason::ToolUse,
        });
        bridge.emit_stream(StreamEvent::Done {
            usage: billed_usage(),
            stop_reason: wakuwaku_harness::StopReason::Stop,
        });
        assert!(matches!(take_usage(&rx), DriverEvent::UsageUpdated { .. }));
        assert!(matches!(take_usage(&rx), DriverEvent::UsageUpdated { .. }));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn cancel_unblocks_a_pending_approval() {
        let (events, rx) = test_event_channel();
        let bridge = Arc::new(ApprovalBridge::new(events));
        bridge.begin_run();
        let cancel = CancelToken::new();
        let worker = {
            let bridge = Arc::clone(&bridge);
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                futures::executor::block_on(bridge.decide(ApprovalRequest {
                    value: call("shell"),
                    cancel,
                }))
            })
        };
        let _ = permission_id(&rx);
        cancel.cancel();
        assert!(matches!(
            worker.join().unwrap(),
            Ok(ApprovalDecision::Cancelled)
        ));
    }

    #[test]
    fn event_bridge_does_not_publish_lifecycle_or_trace_rows() {
        let (events, rx) = test_event_channel();
        let mut bridge = EventBridge::new(
            events,
            ProviderId::new("anthropic"),
            "claude-fable-5".into(),
            200_000,
        );
        bridge.emit(AgentEvent::RunStarted);
        bridge.emit(AgentEvent::TurnStarted);
        bridge.emit(AgentEvent::SteeringInjected { id: 7 });
        bridge.emit(AgentEvent::AssistantDone);
        bridge.emit(AgentEvent::TurnFinished);
        bridge.emit(AgentEvent::RunEnded {
            stop_reason: wakuwaku_harness::StopReason::Stop,
            error_message: None,
        });
        assert!(rx.try_recv().is_err());
    }
}
