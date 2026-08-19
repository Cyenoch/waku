//! In-process trajectory recording and legacy projection.
//!
//! Trace events become provider-neutral ledger rows. Conversion is CPU-only:
//! SQLite lives on the daemon writer thread. Missing timing is left `None`;
//! nothing here invents a duration or TTFT.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use wakuwaku_harness::{
    AssistantMessage, ContentBlock, Message, SessionSnapshot, StopReason, ToolResult, TraceEvent,
    UserMessage, UserPart,
};
use wakuwaku_protocol::model::{ActivityKind, AgentSession, MessageRole};

pub const TRAJECTORY_SCHEMA_VERSION: i64 = 1;
pub const PREVIEW_CHAR_LIMIT: usize = 512;
pub const SEARCH_SOURCE_CHAR_LIMIT: usize = 2048;
pub const SEARCH_OUTPUT_CHAR_LIMIT: usize = 512;

const DETAIL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrajectoryKind {
    System,
    User,
    Context,
    Request,
    Assistant,
    Tool,
}

impl TrajectoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::User => "User",
            Self::Context => "Context",
            Self::Request => "Request",
            Self::Assistant => "Assistant",
            Self::Tool => "Tool",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "System" => Some(Self::System),
            "User" => Some(Self::User),
            "Context" => Some(Self::Context),
            "Request" => Some(Self::Request),
            "Assistant" => Some(Self::Assistant),
            "Tool" => Some(Self::Tool),
            _ => None,
        }
    }

    pub fn lane(self) -> TrajectoryLane {
        match self {
            Self::System | Self::User | Self::Context => TrajectoryLane::Input,
            Self::Request | Self::Assistant => TrajectoryLane::Model,
            Self::Tool => TrajectoryLane::Tools,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrajectoryLane {
    Input,
    Model,
    Tools,
}

impl TrajectoryLane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "Input",
            Self::Model => "Model",
            Self::Tools => "Tools",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Input" => Some(Self::Input),
            "Model" => Some(Self::Model),
            "Tools" => Some(Self::Tools),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrajectoryStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    Unavailable,
}

impl TrajectoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrajectoryAvailability {
    Exact,
    Legacy,
    LegacyPartialMissingSnapshot,
    Unavailable,
    Error,
}

impl TrajectoryAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Legacy => "legacy",
            Self::LegacyPartialMissingSnapshot => "legacy_partial_missing_snapshot",
            Self::Unavailable => "unavailable",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exact" => Some(Self::Exact),
            "legacy" => Some(Self::Legacy),
            "legacy_partial_missing_snapshot" => Some(Self::LegacyPartialMissingSnapshot),
            "unavailable" => Some(Self::Unavailable),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// Safe user-row metadata. Host paths and image bytes stay out.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrajectoryUserInput {
    pub text: String,
    pub display_text: Option<String>,
    pub has_image: bool,
    pub source_metadata_missing: bool,
    pub attachment_labels: Vec<String>,
}

impl TrajectoryUserInput {
    pub fn from_user_message(message: &UserMessage) -> Self {
        let has_image = message
            .parts
            .iter()
            .any(|part| matches!(part, UserPart::Image { .. }));
        Self {
            text: UserMessage::text_of(&message.parts),
            display_text: None,
            has_image,
            source_metadata_missing: has_image,
            attachment_labels: if has_image {
                vec!["image".into()]
            } else {
                Vec::new()
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrajectoryPrompt {
    pub prompt_id: Uuid,
    pub sequence: i64,
    pub fingerprint: String,
    pub system_prompt: Option<String>,
    pub tools_json: String,
    pub options_json: String,
    pub model_hint: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct TrajectoryRecord {
    pub record_id: Uuid,
    pub sequence: i64,
    pub revision: i64,
    pub request_id: Option<Uuid>,
    pub parent_record_id: Option<Uuid>,
    pub prompt_id: Option<Uuid>,
    pub turn_count: i64,
    pub step: i64,
    pub kind: TrajectoryKind,
    pub lane: TrajectoryLane,
    pub status: TrajectoryStatus,
    pub title: String,
    pub preview: String,
    pub search_text: String,
    pub started_at_ms: Option<i64>,
    pub first_token_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub detail_json: String,
}

#[derive(Clone, Debug)]
pub enum TrajectoryOp {
    UpsertPrompt(TrajectoryPrompt),
    UpsertRecord(TrajectoryRecord),
}

#[derive(Clone, Debug, Default)]
pub struct TrajectoryBatch {
    pub session_id: Uuid,
    pub ops: Vec<TrajectoryOp>,
}

impl TrajectoryBatch {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct TrajectorySessionMeta {
    pub session_id: Uuid,
    pub schema_version: i64,
    pub generation: i64,
    pub revision: i64,
    pub next_sequence: i64,
    pub availability: TrajectoryAvailability,
}

#[derive(Clone, Debug)]
pub enum TrajectoryInitSource {
    Snapshot(SessionSnapshot),
    LegacyPartial(Box<AgentSession>),
    Empty,
}

#[derive(Clone, Debug)]
pub enum TrajectoryLiveOp {
    Upsert(Vec<TrajectoryRecord>),
    Remove { record_ids: Vec<Uuid> },
    Reset,
}

#[derive(Clone, Debug)]
pub struct TrajectoryLiveUpdate {
    pub session_id: Uuid,
    pub generation: i64,
    pub revision: i64,
    pub op: TrajectoryLiveOp,
}

#[derive(Clone, Debug)]
pub struct TrajectoryPage {
    pub session_id: Uuid,
    pub availability: TrajectoryAvailability,
    pub generation: i64,
    pub revision: i64,
    pub records: Vec<TrajectoryRecord>,
    pub has_older: bool,
    pub has_newer: bool,
    pub older_cursor: Option<(i64, Uuid)>,
    pub newer_cursor: Option<(i64, Uuid)>,
}

impl TrajectoryPage {
    pub fn empty(session_id: Uuid) -> Self {
        Self {
            session_id,
            availability: TrajectoryAvailability::Exact,
            generation: 1,
            revision: 0,
            records: Vec::new(),
            has_older: false,
            has_newer: false,
            older_cursor: None,
            newer_cursor: None,
        }
    }
}
/// Writer-facing submit/flush used by the recorder. Implemented by
/// `TrajectoryWriter` so this module does not depend on the store.
pub trait TrajectorySubmit: Send + Sync {
    fn submit_batch(&self, batch: TrajectoryBatch) -> Result<(), String>;
    fn flush_session(&self, session_id: Uuid) -> Result<i64, String>;
    fn mark_session_error(&self, session_id: Uuid, message: &str);
}

const RECORDER_BOUND: usize = 512;

enum RecorderCommand {
    User {
        session_id: Uuid,
        user: TrajectoryUserInput,
    },
    Steer {
        session_id: Uuid,
        steer: TrajectoryUserInput,
    },
    Trace {
        session_id: Uuid,
        event: TraceEvent,
    },
    Finish {
        session_id: Uuid,
        ack: crossbeam_channel::Sender<Result<i64, String>>,
    },
    Discard {
        session_id: Uuid,
    },
    Shutdown {
        ack: crossbeam_channel::Sender<Result<i64, String>>,
    },
}

/// Bounded nonblocking handoff from the harness thread to a daemon recorder.
pub struct TraceHandoff {
    tx: crossbeam_channel::Sender<RecorderCommand>,
    failed: Arc<Mutex<HashSet<Uuid>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl TraceHandoff {
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::bounded(RECORDER_BOUND);
        drop(rx);
        Self {
            tx,
            failed: Arc::new(Mutex::new(HashSet::new())),
            thread: Mutex::new(None),
        }
    }

    pub fn spawn(submit: Arc<dyn TrajectorySubmit>) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(RECORDER_BOUND);
        let failed = Arc::new(Mutex::new(HashSet::new()));
        let failed_worker = Arc::clone(&failed);
        let handle = std::thread::Builder::new()
            .name("waku-trajectory-recorder".into())
            .spawn(move || recorder_loop(rx, submit, failed_worker))
            .expect("start trajectory recorder");
        Self {
            tx,
            failed,
            thread: Mutex::new(Some(handle)),
        }
    }

    pub fn sink(&self, session_id: Uuid) -> HandoffSink {
        HandoffSink {
            session_id,
            tx: self.tx.clone(),
            failed: Arc::clone(&self.failed),
        }
    }

    pub fn stage_user(&self, session_id: Uuid, user: TrajectoryUserInput) {
        if self
            .tx
            .send(RecorderCommand::User { session_id, user })
            .is_err()
        {
            self.failed.lock().insert(session_id);
        }
    }

    pub fn stage_steer(&self, session_id: Uuid, steer: TrajectoryUserInput) {
        if self
            .tx
            .send(RecorderCommand::Steer { session_id, steer })
            .is_err()
        {
            self.failed.lock().insert(session_id);
        }
    }

    pub fn finish_and_flush(&self, session_id: Uuid) -> Result<i64, String> {
        let (ack, rx) = crossbeam_channel::bounded(1);
        self.tx
            .send(RecorderCommand::Finish { session_id, ack })
            .map_err(|_| "trajectory recorder is unavailable".to_owned())?;
        rx.recv_timeout(default_flush_timeout())
            .map_err(|_| "trajectory recorder timed out".to_owned())?
    }

    pub fn discard(&self, session_id: Uuid) {
        self.failed.lock().remove(&session_id);
        let _ = self.tx.send(RecorderCommand::Discard { session_id });
    }
}

impl Default for TraceHandoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TraceHandoff {
    fn drop(&mut self) {
        if let Some(handle) = self.thread.lock().take() {
            let (ack, rx) = crossbeam_channel::bounded(1);
            let _ = self.tx.send(RecorderCommand::Shutdown { ack });
            let _ = rx.recv_timeout(Duration::from_secs(5));
            let _ = handle.join();
        }
    }
}

/// Harness-thread sink: `try_send` only. Overflow marks the session failed.
pub struct HandoffSink {
    session_id: Uuid,
    tx: crossbeam_channel::Sender<RecorderCommand>,
    failed: Arc<Mutex<HashSet<Uuid>>>,
}

impl wakuwaku_harness::TraceSink for HandoffSink {
    fn emit(&mut self, event: TraceEvent) {
        if self.failed.lock().contains(&self.session_id) {
            return;
        }
        if self
            .tx
            .try_send(RecorderCommand::Trace {
                session_id: self.session_id,
                event,
            })
            .is_err()
        {
            self.failed.lock().insert(self.session_id);
        }
    }
}

fn recorder_loop(
    rx: crossbeam_channel::Receiver<RecorderCommand>,
    submit: Arc<dyn TrajectorySubmit>,
    failed: Arc<Mutex<HashSet<Uuid>>>,
) {
    let mut sessions: HashMap<Uuid, DriveRecorder> = HashMap::new();
    while let Ok(command) = rx.recv() {
        match command {
            RecorderCommand::User { session_id, user } => {
                sessions
                    .entry(session_id)
                    .or_insert_with(|| DriveRecorder::empty(session_id))
                    .set_user(user);
            }
            RecorderCommand::Steer { session_id, steer } => {
                sessions
                    .entry(session_id)
                    .or_insert_with(|| DriveRecorder::empty(session_id))
                    .push_steer(steer);
            }
            RecorderCommand::Trace { session_id, event } => {
                if failed.lock().contains(&session_id) {
                    continue;
                }
                let recorder = sessions
                    .entry(session_id)
                    .or_insert_with(|| DriveRecorder::empty(session_id));
                recorder.push(event);
                if submit_taken(&*submit, session_id, recorder.take_ops()).is_err() {
                    failed.lock().insert(session_id);
                }
            }
            RecorderCommand::Finish { session_id, ack } => {
                if let Some(recorder) = sessions.get_mut(&session_id)
                    && submit_taken(&*submit, session_id, recorder.finish_remaining()).is_err()
                {
                    failed.lock().insert(session_id);
                }
                if failed.lock().contains(&session_id) {
                    submit.mark_session_error(session_id, "trace recorder overflow or disconnect");
                }
                let _ = ack.send(submit.flush_session(session_id));
            }
            RecorderCommand::Discard { session_id } => {
                sessions.remove(&session_id);
                failed.lock().remove(&session_id);
            }
            RecorderCommand::Shutdown { ack } => {
                let _ = ack.send(Ok(0));
                break;
            }
        }
    }
}

fn submit_taken(
    submit: &dyn TrajectorySubmit,
    session_id: Uuid,
    ops: Vec<TrajectoryOp>,
) -> Result<(), String> {
    if ops.is_empty() {
        return Ok(());
    }
    submit.submit_batch(TrajectoryBatch { session_id, ops })
}

pub fn record_drive(
    session_id: Uuid,
    user: Option<TrajectoryUserInput>,
    steers: Vec<TrajectoryUserInput>,
    events: Vec<TraceEvent>,
) -> TrajectoryBatch {
    let mut recorder = DriveRecorder::empty(session_id);
    if let Some(user) = user {
        recorder.set_user(user);
    }
    for steer in steers {
        recorder.push_steer(steer);
    }
    for event in events {
        recorder.push(event);
    }
    recorder.finish()
}
pub fn project_snapshot(session_id: Uuid, snapshot: &SessionSnapshot) -> TrajectoryBatch {
    let mut builder = LegacyBuilder::new(session_id);
    if let Some(system) = snapshot.system_prompt.as_deref() {
        builder.push_system(system, unix_time_ms());
    }
    for message in &snapshot.messages {
        match message {
            Message::User(user) => builder.push_user(&TrajectoryUserInput::from_user_message(user)),
            Message::Assistant(assistant) => builder.push_assistant(assistant),
            Message::ToolResult(result) => builder.push_tool_result(result),
        }
    }
    builder.finish()
}

pub fn project_legacy_session(session_id: Uuid, session: &AgentSession) -> TrajectoryBatch {
    let mut builder = LegacyBuilder::new(session_id);
    let turn_by_id = session
        .turns
        .iter()
        .map(|turn| (turn.id, turn.turn_count as i64))
        .collect::<HashMap<_, _>>();
    for message in &session.messages {
        let turn = message
            .turn_id
            .and_then(|id| turn_by_id.get(&id).copied())
            .unwrap_or(0);
        match message.role {
            MessageRole::System => builder.push_system(&message.content, unix_time_ms()),
            MessageRole::User => {
                builder.turn_count = if turn > 0 {
                    turn
                } else {
                    builder.turn_count + 1
                };
                builder.step = 0;
                let mut user = TrajectoryUserInput {
                    text: message.content.clone(),
                    display_text: message.display_content.clone(),
                    has_image: message.attachments.iter().any(|item| item.is_image),
                    source_metadata_missing: false,
                    attachment_labels: message
                        .attachments
                        .iter()
                        .map(|item| item.name.clone())
                        .collect(),
                };
                if user.has_image && user.attachment_labels.is_empty() {
                    user.source_metadata_missing = true;
                }
                builder.push_user(&user);
            }
            MessageRole::Assistant => {
                builder.turn_count = if turn > 0 {
                    turn
                } else {
                    builder.turn_count.max(1)
                };
                builder.step += 1;
                builder.push_plain_assistant(&message.content);
            }
        }
    }
    for block in &session.transcript_blocks {
        let turn = block
            .turn_id
            .and_then(|id| turn_by_id.get(&id).copied())
            .unwrap_or(builder.turn_count.max(1));
        for activity in &block.activities {
            if activity.kind == ActivityKind::Reasoning {
                continue;
            }
            builder.push_activity(turn, activity);
        }
    }
    builder.finish()
}

struct DriveRecorder {
    session_id: Uuid,
    now_instant: Instant,
    now_unix: i64,
    user: Option<TrajectoryUserInput>,
    steers: VecDeque<TrajectoryUserInput>,
    user_emitted: bool,
    current_prompt: Option<Uuid>,
    current_request: Option<RequestCursor>,
    ops: Vec<TrajectoryOp>,
}

#[derive(Clone, Copy)]
struct RequestCursor {
    record_id: Uuid,
    started_at: Instant,
    started_at_ms: i64,
    first_token_at_ms: Option<i64>,
    turn: usize,
    step: usize,
    completed: bool,
}

impl DriveRecorder {
    fn empty(session_id: Uuid) -> Self {
        Self {
            session_id,
            now_instant: Instant::now(),
            now_unix: unix_time_ms(),
            user: None,
            steers: VecDeque::new(),
            user_emitted: false,
            current_prompt: None,
            current_request: None,
            ops: Vec::new(),
        }
    }

    fn set_user(&mut self, user: TrajectoryUserInput) {
        if !self.user_emitted {
            self.user = Some(user);
        }
    }

    fn push_steer(&mut self, steer: TrajectoryUserInput) {
        self.steers.push_back(steer);
    }

    fn take_ops(&mut self) -> Vec<TrajectoryOp> {
        std::mem::take(&mut self.ops)
    }

    fn finish_remaining(&mut self) -> Vec<TrajectoryOp> {
        if !self.user_emitted {
            self.emit_user(1);
        }
        self.take_ops()
    }

    fn finish(mut self) -> TrajectoryBatch {
        if !self.user_emitted {
            self.emit_user(1);
        }
        TrajectoryBatch {
            session_id: self.session_id,
            ops: self.ops,
        }
    }

    fn push(&mut self, event: TraceEvent) {
        match event {
            TraceEvent::PromptPrepared {
                system_prompt,
                tools_json,
                options_json,
                model_hint,
            } => self.prompt_prepared(system_prompt, tools_json, options_json, model_hint),
            TraceEvent::RequestStart {
                visible_turn,
                step,
                started_at,
            } => self.request_start(visible_turn, step, started_at),
            TraceEvent::RequestFirstToken {
                visible_turn,
                step,
                at,
            } => self.request_first_token(visible_turn, step, at),
            TraceEvent::RequestFailed {
                visible_turn,
                step,
                failed_at,
                error,
            } => self.request_failed(visible_turn, step, failed_at, error),
            TraceEvent::SteeringInjected { id } => self.steering(id),
            TraceEvent::ToolExecution {
                call_id,
                started_at,
                finished_at,
                result_preview,
            } => self.tool(call_id, started_at, finished_at, result_preview),
            TraceEvent::AssistantDone(message) => self.assistant_done(message),
        }
    }

    fn prompt_prepared(
        &mut self,
        system_prompt: Option<String>,
        tools_json: Arc<str>,
        options_json: Arc<str>,
        model_hint: String,
    ) {
        let created_at_ms = self.now_unix;
        let fingerprint = prompt_fingerprint(
            system_prompt.as_deref(),
            &tools_json,
            &options_json,
            &model_hint,
        );
        let prompt_id = Uuid::new_v4();
        self.ops.push(TrajectoryOp::UpsertPrompt(TrajectoryPrompt {
            prompt_id,
            sequence: 0,
            fingerprint,
            system_prompt: system_prompt.clone(),
            tools_json: tools_json.to_string(),
            options_json: options_json.to_string(),
            model_hint: model_hint.clone(),
            created_at_ms,
        }));
        self.current_prompt = Some(prompt_id);
        if let Some(system) = system_prompt
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            self.push_record(
                base_record(
                    TrajectoryKind::System,
                    0,
                    0,
                    "System prompt",
                    system,
                    system,
                    json!({
                        "v": DETAIL_VERSION,
                        "kind": "system",
                        "model_hint": model_hint,
                    }),
                )
                .with_prompt(prompt_id)
                .with_status(TrajectoryStatus::Completed)
                .with_started(created_at_ms)
                .with_completed(created_at_ms, None),
            );
        }
        self.emit_user(1);
    }

    fn emit_user(&mut self, turn: i64) {
        if self.user_emitted {
            return;
        }
        let Some(user) = self.user.take() else {
            return;
        };
        self.user_emitted = true;
        let preview_source = user.display_text.as_deref().unwrap_or(user.text.as_str());
        let title = first_line(preview_source);
        let title = if title.is_empty() {
            "User".to_owned()
        } else {
            title
        };
        let mut search = preview_source.to_owned();
        if !user.attachment_labels.is_empty() {
            search.push('\n');
            search.push_str(&user.attachment_labels.join(" "));
        }
        self.push_record(
            base_record(
                TrajectoryKind::User,
                turn,
                0,
                &title,
                preview_source,
                &search,
                json!({
                    "v": DETAIL_VERSION,
                    "kind": "user",
                    "text": user.text,
                    "display_text": user.display_text,
                    "has_image": user.has_image,
                    "source_metadata_missing": user.source_metadata_missing,
                    "attachments": user.attachment_labels,
                }),
            )
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Completed)
            .with_started(self.now_unix)
            .with_completed(self.now_unix, None),
        );
    }

    fn request_start(&mut self, visible_turn: usize, step: usize, started_at: Instant) {
        self.emit_user(visible_turn.max(1) as i64);
        let started_at_ms = self.unix_of(started_at);
        let record_id = Uuid::new_v4();
        self.current_request = Some(RequestCursor {
            record_id,
            started_at,
            started_at_ms,
            first_token_at_ms: None,
            turn: visible_turn,
            step,
            completed: false,
        });
        self.push_record(
            base_record(
                TrajectoryKind::Request,
                visible_turn as i64,
                step as i64,
                "Request",
                "",
                "",
                json!({ "v": DETAIL_VERSION, "kind": "request" }),
            )
            .with_id(record_id)
            .with_request(record_id)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Running)
            .with_started(started_at_ms),
        );
    }

    fn request_first_token(&mut self, visible_turn: usize, step: usize, at: Instant) {
        let Some(cursor) = self.current_request else {
            return;
        };
        if cursor.turn != visible_turn || cursor.step != step || cursor.first_token_at_ms.is_some()
        {
            return;
        }
        let first_token_at_ms = instant_to_unix_ms(at, self.now_instant, self.now_unix);
        let ttft_ms = millis_between(cursor.started_at, at);
        if let Some(current) = self.current_request.as_mut() {
            current.first_token_at_ms = Some(first_token_at_ms);
        }
        self.push_record(
            base_record(
                TrajectoryKind::Request,
                visible_turn as i64,
                step as i64,
                "Request",
                "",
                "",
                json!({ "v": DETAIL_VERSION, "kind": "request" }),
            )
            .with_id(cursor.record_id)
            .with_request(cursor.record_id)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Running)
            .with_started(cursor.started_at_ms)
            .with_first_token(first_token_at_ms, ttft_ms),
        );
    }

    fn request_failed(
        &mut self,
        visible_turn: usize,
        step: usize,
        failed_at: Instant,
        error: String,
    ) {
        self.emit_user(visible_turn.max(1) as i64);
        let Some(cursor) = self.current_request else {
            return;
        };
        if cursor.turn != visible_turn || cursor.step != step {
            return;
        }
        let completed_at_ms = instant_to_unix_ms(failed_at, self.now_instant, self.now_unix);
        let duration_ms = millis_between(cursor.started_at, failed_at);
        if let Some(current) = self.current_request.as_mut() {
            current.completed = true;
        }
        self.push_record(
            base_record(
                TrajectoryKind::Request,
                visible_turn as i64,
                step as i64,
                "Request",
                &error,
                &error,
                json!({ "v": DETAIL_VERSION, "kind": "request", "error": error }),
            )
            .with_id(cursor.record_id)
            .with_request(cursor.record_id)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Failed)
            .with_started(cursor.started_at_ms)
            .with_first_token_opt(cursor.first_token_at_ms, None)
            .with_completed(completed_at_ms, Some(duration_ms)),
        );
    }

    fn steering(&mut self, id: u64) {
        let steer = self.steers.pop_front();
        let text = steer
            .as_ref()
            .map(|input| {
                input
                    .display_text
                    .as_deref()
                    .unwrap_or(&input.text)
                    .to_owned()
            })
            .unwrap_or_default();
        let turn = self
            .current_request
            .map(|cursor| cursor.turn as i64)
            .unwrap_or(1);
        let step = self
            .current_request
            .map(|cursor| cursor.step as i64)
            .unwrap_or(0);
        let parent = self.current_request.map(|cursor| cursor.record_id);
        self.push_record(
            base_record(
                TrajectoryKind::Context,
                turn,
                step,
                "Steering",
                &text,
                &text,
                json!({ "v": DETAIL_VERSION, "kind": "context", "steering_id": id }),
            )
            .with_parent_opt(parent)
            .with_request_opt(parent)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Completed)
            .with_started(self.now_unix)
            .with_completed(self.now_unix, None),
        );
    }

    fn tool(
        &mut self,
        call_id: String,
        started_at: Instant,
        finished_at: Instant,
        result_preview: Arc<str>,
    ) {
        let parent = self.current_request.map(|cursor| cursor.record_id);
        let turn = self
            .current_request
            .map(|cursor| cursor.turn as i64)
            .unwrap_or(1);
        let step = self
            .current_request
            .map(|cursor| cursor.step as i64)
            .unwrap_or(0);
        let started_at_ms = self.unix_of(started_at);
        let completed_at_ms = self.unix_of(finished_at);
        let duration_ms = millis_between(started_at, finished_at);
        let preview = result_preview.as_ref();
        self.push_record(
            base_record(
                TrajectoryKind::Tool,
                turn,
                step,
                &call_id,
                preview,
                preview,
                json!({
                    "v": DETAIL_VERSION,
                    "kind": "tool",
                    "call_id": call_id,
                }),
            )
            .with_parent_opt(parent)
            .with_request_opt(parent)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Completed)
            .with_started(started_at_ms)
            .with_completed(completed_at_ms, Some(duration_ms)),
        );
    }

    fn assistant_done(&mut self, message: Arc<AssistantMessage>) {
        let cursor = self.current_request;
        let turn = cursor.map(|item| item.turn as i64).unwrap_or(1);
        let step = cursor.map(|item| item.step as i64).unwrap_or(0);
        let parent = cursor.map(|item| item.record_id);
        let text = assistant_text(&message);
        let thinking = assistant_thinking(&message);
        let mut search = text.clone();
        if !thinking.is_empty() {
            search.push('\n');
            search.push_str(&thinking);
        }
        let failed = message.error_message.is_some()
            || matches!(message.stop_reason, StopReason::Error | StopReason::Aborted);
        let status = if failed {
            if matches!(message.stop_reason, StopReason::Aborted) {
                TrajectoryStatus::Cancelled
            } else {
                TrajectoryStatus::Failed
            }
        } else {
            TrajectoryStatus::Completed
        };
        let preview = if text.is_empty() {
            message.error_message.clone().unwrap_or_default()
        } else {
            text.clone()
        };
        self.push_record(
            base_record(
                TrajectoryKind::Assistant,
                turn,
                step,
                "Assistant",
                &preview,
                &search,
                assistant_detail(&message),
            )
            .with_parent_opt(parent)
            .with_request_opt(parent)
            .with_prompt_opt(self.current_prompt)
            .with_status(status),
        );
        if let Some(cursor) = self.current_request.filter(|cursor| !cursor.completed) {
            let finished_at = Instant::now();
            let completed_at_ms = self.unix_of(finished_at);
            let duration_ms = millis_between(cursor.started_at, finished_at);
            if let Some(current) = self.current_request.as_mut() {
                current.completed = true;
            }
            self.push_record(
                base_record(
                    TrajectoryKind::Request,
                    turn,
                    step,
                    "Request",
                    "",
                    "",
                    json!({
                        "v": DETAIL_VERSION,
                        "kind": "request",
                        "model": message.model,
                        "provider": message.provider,
                    }),
                )
                .with_id(cursor.record_id)
                .with_request(cursor.record_id)
                .with_prompt_opt(self.current_prompt)
                .with_status(status)
                .with_started(cursor.started_at_ms)
                .with_first_token_opt(cursor.first_token_at_ms, None)
                .with_completed(completed_at_ms, Some(duration_ms)),
            );
        }
    }

    fn push_record(&mut self, record: TrajectoryRecord) {
        self.ops.push(TrajectoryOp::UpsertRecord(record));
    }

    fn unix_of(&self, at: Instant) -> i64 {
        instant_to_unix_ms(at, self.now_instant, self.now_unix)
    }
}

struct LegacyBuilder {
    session_id: Uuid,
    turn_count: i64,
    step: i64,
    current_request: Option<Uuid>,
    current_prompt: Option<Uuid>,
    ops: Vec<TrajectoryOp>,
}

impl LegacyBuilder {
    fn new(session_id: Uuid) -> Self {
        Self {
            session_id,
            turn_count: 0,
            step: 0,
            current_request: None,
            current_prompt: None,
            ops: Vec::new(),
        }
    }

    fn push_system(&mut self, text: &str, created_at_ms: i64) {
        if text.trim().is_empty() {
            return;
        }
        let prompt_id = Uuid::new_v4();
        self.current_prompt = Some(prompt_id);
        self.ops.push(TrajectoryOp::UpsertPrompt(TrajectoryPrompt {
            prompt_id,
            sequence: 0,
            fingerprint: prompt_fingerprint(Some(text), "", "", ""),
            system_prompt: Some(text.to_owned()),
            tools_json: String::new(),
            options_json: String::new(),
            model_hint: String::new(),
            created_at_ms,
        }));
        self.ops.push(TrajectoryOp::UpsertRecord(
            base_record(
                TrajectoryKind::System,
                0,
                0,
                "System prompt",
                text,
                text,
                json!({ "v": DETAIL_VERSION, "kind": "system" }),
            )
            .with_prompt(prompt_id)
            .with_status(TrajectoryStatus::Completed),
        ));
    }

    fn push_user(&mut self, user: &TrajectoryUserInput) {
        self.turn_count += 1;
        self.step = 0;
        self.current_request = None;
        let preview_source = user.display_text.as_deref().unwrap_or(user.text.as_str());
        let title = first_line(preview_source);
        let title = if title.is_empty() {
            "User".to_owned()
        } else {
            title
        };
        self.ops.push(TrajectoryOp::UpsertRecord(
            base_record(
                TrajectoryKind::User,
                self.turn_count,
                0,
                &title,
                preview_source,
                preview_source,
                json!({
                    "v": DETAIL_VERSION,
                    "kind": "user",
                    "text": user.text,
                    "display_text": user.display_text,
                    "has_image": user.has_image,
                    "source_metadata_missing": user.source_metadata_missing,
                    "attachments": user.attachment_labels,
                }),
            )
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Completed),
        ));
    }

    fn push_assistant(&mut self, assistant: &AssistantMessage) {
        self.step += 1;
        let request_id = Uuid::new_v4();
        self.current_request = Some(request_id);
        self.ops.push(TrajectoryOp::UpsertRecord(
            base_record(
                TrajectoryKind::Request,
                self.turn_count.max(1),
                self.step,
                "Request",
                "",
                "",
                json!({
                    "v": DETAIL_VERSION,
                    "kind": "request",
                    "model": assistant.model,
                    "provider": assistant.provider,
                }),
            )
            .with_id(request_id)
            .with_request(request_id)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Completed),
        ));
        let text = assistant_text(assistant);
        self.ops.push(TrajectoryOp::UpsertRecord(
            base_record(
                TrajectoryKind::Assistant,
                self.turn_count.max(1),
                self.step,
                "Assistant",
                &text,
                &text,
                assistant_detail(assistant),
            )
            .with_parent(request_id)
            .with_request(request_id)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Completed),
        ));
    }

    fn push_plain_assistant(&mut self, text: &str) {
        self.step += 1;
        let request_id = Uuid::new_v4();
        self.current_request = Some(request_id);
        self.ops.push(TrajectoryOp::UpsertRecord(
            base_record(
                TrajectoryKind::Request,
                self.turn_count.max(1),
                self.step,
                "Request",
                "",
                "",
                json!({ "v": DETAIL_VERSION, "kind": "request" }),
            )
            .with_id(request_id)
            .with_request(request_id)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Completed),
        ));
        self.ops.push(TrajectoryOp::UpsertRecord(
            base_record(
                TrajectoryKind::Assistant,
                self.turn_count.max(1),
                self.step,
                "Assistant",
                text,
                text,
                json!({ "v": DETAIL_VERSION, "kind": "assistant", "text": text }),
            )
            .with_parent(request_id)
            .with_request(request_id)
            .with_prompt_opt(self.current_prompt)
            .with_status(TrajectoryStatus::Completed),
        ));
    }

    fn push_tool_result(&mut self, result: &ToolResult) {
        let parent = self.current_request;
        let preview = tool_result_text(result);
        self.ops.push(TrajectoryOp::UpsertRecord(
            base_record(
                TrajectoryKind::Tool,
                self.turn_count.max(1),
                self.step,
                &result.tool_name,
                &preview,
                &preview,
                json!({
                    "v": DETAIL_VERSION,
                    "kind": "tool",
                    "call_id": result.tool_call_id,
                    "name": result.tool_name,
                    "is_error": result.is_error,
                }),
            )
            .with_parent_opt(parent)
            .with_request_opt(parent)
            .with_prompt_opt(self.current_prompt)
            .with_status(if result.is_error {
                TrajectoryStatus::Failed
            } else {
                TrajectoryStatus::Completed
            }),
        ));
    }

    fn push_activity(&mut self, turn: i64, activity: &wakuwaku_protocol::model::ActivityItem) {
        let preview = activity
            .output
            .as_deref()
            .or(activity.detail.as_deref())
            .unwrap_or("");
        self.ops.push(TrajectoryOp::UpsertRecord(
            base_record(
                TrajectoryKind::Tool,
                turn,
                self.step.max(1),
                &activity.title,
                preview,
                preview,
                json!({
                    "v": DETAIL_VERSION,
                    "kind": "tool",
                    "activity_id": activity.id,
                    "source_id": activity.source_id,
                }),
            )
            .with_parent_opt(self.current_request)
            .with_request_opt(self.current_request)
            .with_prompt_opt(self.current_prompt)
            .with_status(if activity.failed {
                TrajectoryStatus::Failed
            } else {
                TrajectoryStatus::Completed
            }),
        ));
    }

    fn finish(self) -> TrajectoryBatch {
        TrajectoryBatch {
            session_id: self.session_id,
            ops: self.ops,
        }
    }
}

fn base_record(
    kind: TrajectoryKind,
    turn_count: i64,
    step: i64,
    title: &str,
    preview: &str,
    search: &str,
    detail: Value,
) -> TrajectoryRecord {
    TrajectoryRecord {
        record_id: Uuid::new_v4(),
        sequence: 0,
        revision: 0,
        request_id: None,
        parent_record_id: None,
        prompt_id: None,
        turn_count,
        step,
        kind,
        lane: kind.lane(),
        status: TrajectoryStatus::Pending,
        title: title.to_owned(),
        preview: truncate_chars(preview, PREVIEW_CHAR_LIMIT),
        search_text: bound_search(search, ""),
        started_at_ms: None,
        first_token_at_ms: None,
        completed_at_ms: None,
        duration_ms: None,
        ttft_ms: None,
        detail_json: detail.to_string(),
    }
}

impl TrajectoryRecord {
    fn with_id(mut self, id: Uuid) -> Self {
        self.record_id = id;
        self
    }

    fn with_request(mut self, id: Uuid) -> Self {
        self.request_id = Some(id);
        self
    }

    fn with_request_opt(mut self, id: Option<Uuid>) -> Self {
        self.request_id = id;
        self
    }

    fn with_parent(mut self, id: Uuid) -> Self {
        self.parent_record_id = Some(id);
        self
    }

    fn with_parent_opt(mut self, id: Option<Uuid>) -> Self {
        self.parent_record_id = id;
        self
    }

    fn with_prompt(mut self, id: Uuid) -> Self {
        self.prompt_id = Some(id);
        self
    }

    fn with_prompt_opt(mut self, id: Option<Uuid>) -> Self {
        self.prompt_id = id;
        self
    }

    fn with_status(mut self, status: TrajectoryStatus) -> Self {
        self.status = status;
        self
    }

    fn with_started(mut self, started_at_ms: i64) -> Self {
        self.started_at_ms = Some(started_at_ms);
        self
    }

    fn with_first_token(mut self, first_token_at_ms: i64, ttft_ms: i64) -> Self {
        self.first_token_at_ms = Some(first_token_at_ms);
        self.ttft_ms = Some(ttft_ms);
        self
    }

    fn with_first_token_opt(
        mut self,
        first_token_at_ms: Option<i64>,
        ttft_ms: Option<i64>,
    ) -> Self {
        self.first_token_at_ms = first_token_at_ms;
        if ttft_ms.is_some() {
            self.ttft_ms = ttft_ms;
        } else if let (Some(start), Some(first)) = (self.started_at_ms, first_token_at_ms) {
            self.ttft_ms = Some(first.saturating_sub(start));
        }
        self
    }

    fn with_completed(mut self, completed_at_ms: i64, duration_ms: Option<i64>) -> Self {
        self.completed_at_ms = Some(completed_at_ms);
        self.duration_ms = duration_ms;
        self
    }
}

fn prompt_fingerprint(system: Option<&str>, tools: &str, options: &str, model: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(system.unwrap_or("").as_bytes());
    hasher.update([0]);
    hasher.update(tools.as_bytes());
    hasher.update([0]);
    hasher.update(options.as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn assistant_text(message: &AssistantMessage) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text(block) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block.text);
        }
    }
    text
}

fn assistant_thinking(message: &AssistantMessage) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Thinking(block) = block {
            if block.redacted {
                continue;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block.thinking);
        }
    }
    text
}

fn assistant_detail(message: &AssistantMessage) -> Value {
    let blocks = message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(block) => json!({
                "type": "text",
                "text": block.text,
            }),
            ContentBlock::Thinking(block) => json!({
                "type": "thinking",
                "text": block.thinking,
                "redacted": block.redacted,
            }),
            ContentBlock::ToolCall(call) => json!({
                "type": "tool_call",
                "id": call.id,
                "name": call.name,
                "arguments": call.arguments,
            }),
        })
        .collect::<Vec<_>>();
    json!({
        "v": DETAIL_VERSION,
        "kind": "assistant",
        "model": message.model,
        "provider": message.provider,
        "usage": {
            "input": message.usage.input,
            "output": message.usage.output,
            "cache_read": message.usage.cache_read,
            "cache_write": message.usage.cache_write,
            "reasoning": message.usage.reasoning,
        },
        "stop_reason": stop_reason_name(message.stop_reason),
        "error_message": message.error_message,
        "blocks": blocks,
    })
}

fn stop_reason_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Pending => "pending",
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "tool_use",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        StopReason::Deferred => "deferred",
    }
}

fn tool_result_text(result: &ToolResult) -> String {
    let mut text = String::new();
    for part in &result.content {
        if let wakuwaku_harness::ToolResultPart::Text(part) = part {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
        }
    }
    text
}

fn first_line(text: &str) -> String {
    truncate_chars(text.lines().next().unwrap_or("").trim(), 80)
}

fn bound_search(source: &str, output: &str) -> String {
    let mut text = truncate_chars(source, SEARCH_SOURCE_CHAR_LIMIT);
    if !output.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&truncate_chars(output, SEARCH_OUTPUT_CHAR_LIMIT));
    }
    text
}

fn truncate_chars(input: &str, max: usize) -> String {
    input.chars().take(max).collect()
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn instant_to_unix_ms(at: Instant, now_instant: Instant, now_unix: i64) -> i64 {
    if at >= now_instant {
        now_unix.saturating_add(at.duration_since(now_instant).as_millis() as i64)
    } else {
        now_unix.saturating_sub(now_instant.duration_since(at).as_millis() as i64)
    }
}

fn millis_between(start: Instant, end: Instant) -> i64 {
    if end >= start {
        end.duration_since(start).as_millis() as i64
    } else {
        0
    }
}

pub fn clamp_page_limit(limit: Option<u32>) -> u32 {
    wakuwaku_protocol::clamp_trajectory_page_limit(limit)
}

pub fn default_flush_timeout() -> Duration {
    Duration::from_secs(30)
}

pub fn to_row_summary(record: &TrajectoryRecord) -> wakuwaku_protocol::TrajectoryRowSummary {
    wakuwaku_protocol::TrajectoryRowSummary {
        record_id: record.record_id,
        sequence: record.sequence.max(0) as u64,
        revision: record.revision.max(0) as u64,
        request_id: record.request_id,
        parent_record_id: record.parent_record_id,
        prompt_id: record.prompt_id,
        turn_count: record.turn_count.max(0) as u32,
        step: record.step.max(0) as u32,
        kind: wire_kind(record.kind),
        lane: wire_lane(record.lane),
        status: wire_status(record.status),
        title: record.title.clone(),
        preview: record.preview.clone(),
        search_text: wakuwaku_protocol::bound_search_text(&record.search_text, ""),
        started_at_ms: record.started_at_ms,
        first_token_at_ms: record.first_token_at_ms,
        completed_at_ms: record.completed_at_ms,
        duration_ms: record.duration_ms,
        ttft_ms: record.ttft_ms,
    }
}

pub fn to_page_response(page: &TrajectoryPage) -> wakuwaku_protocol::TrajectoryResponse {
    wakuwaku_protocol::TrajectoryResponse::Page {
        availability: wire_availability(page.availability),
        generation: page.generation.max(0) as u64,
        revision: page.revision.max(0) as u64,
        rows: page.records.iter().map(to_row_summary).collect(),
        older: page.older_cursor.map(
            |(sequence, record_id)| wakuwaku_protocol::TrajectoryCursor {
                sequence: sequence.max(0) as u64,
                record_id,
            },
        ),
        newer: page.newer_cursor.map(
            |(sequence, record_id)| wakuwaku_protocol::TrajectoryCursor {
                sequence: sequence.max(0) as u64,
                record_id,
            },
        ),
        has_older: page.has_older,
        has_newer: page.has_newer,
    }
}

pub fn to_wire_live_updates(
    update: &TrajectoryLiveUpdate,
) -> Vec<wakuwaku_protocol::TrajectoryLiveUpdate> {
    let generation = update.generation.max(0) as u64;
    let revision = update.revision.max(0) as u64;
    match &update.op {
        TrajectoryLiveOp::Upsert(records) => records
            .iter()
            .map(|record| wakuwaku_protocol::TrajectoryLiveUpdate::Upsert {
                generation,
                revision,
                row: Box::new(to_row_summary(record)),
            })
            .collect(),
        TrajectoryLiveOp::Remove { record_ids } => record_ids
            .iter()
            .copied()
            .map(
                |record_id| wakuwaku_protocol::TrajectoryLiveUpdate::Remove {
                    generation,
                    revision,
                    record_id,
                },
            )
            .collect(),
        TrajectoryLiveOp::Reset => vec![wakuwaku_protocol::TrajectoryLiveUpdate::Reset {
            generation,
            revision,
        }],
    }
}

fn wire_kind(kind: TrajectoryKind) -> wakuwaku_protocol::TrajectoryKind {
    match kind {
        TrajectoryKind::System => wakuwaku_protocol::TrajectoryKind::System,
        TrajectoryKind::User => wakuwaku_protocol::TrajectoryKind::User,
        TrajectoryKind::Context => wakuwaku_protocol::TrajectoryKind::Context,
        TrajectoryKind::Request => wakuwaku_protocol::TrajectoryKind::Request,
        TrajectoryKind::Assistant => wakuwaku_protocol::TrajectoryKind::Assistant,
        TrajectoryKind::Tool => wakuwaku_protocol::TrajectoryKind::Tool,
    }
}

fn wire_lane(lane: TrajectoryLane) -> wakuwaku_protocol::TrajectoryLane {
    match lane {
        TrajectoryLane::Input => wakuwaku_protocol::TrajectoryLane::Input,
        TrajectoryLane::Model => wakuwaku_protocol::TrajectoryLane::Model,
        TrajectoryLane::Tools => wakuwaku_protocol::TrajectoryLane::Tools,
    }
}

fn wire_status(status: TrajectoryStatus) -> wakuwaku_protocol::TrajectoryStatus {
    match status {
        TrajectoryStatus::Pending => wakuwaku_protocol::TrajectoryStatus::Pending,
        TrajectoryStatus::Running => wakuwaku_protocol::TrajectoryStatus::Running,
        TrajectoryStatus::Completed => wakuwaku_protocol::TrajectoryStatus::Completed,
        TrajectoryStatus::Failed => wakuwaku_protocol::TrajectoryStatus::Failed,
        TrajectoryStatus::Cancelled => wakuwaku_protocol::TrajectoryStatus::Cancelled,
        TrajectoryStatus::Unavailable => wakuwaku_protocol::TrajectoryStatus::Unavailable,
    }
}

fn wire_availability(
    availability: TrajectoryAvailability,
) -> wakuwaku_protocol::TrajectoryAvailability {
    match availability {
        TrajectoryAvailability::Exact => wakuwaku_protocol::TrajectoryAvailability::Exact,
        TrajectoryAvailability::Legacy => wakuwaku_protocol::TrajectoryAvailability::Legacy,
        TrajectoryAvailability::LegacyPartialMissingSnapshot => {
            wakuwaku_protocol::TrajectoryAvailability::LegacyPartialMissingSnapshot
        }
        TrajectoryAvailability::Unavailable => {
            wakuwaku_protocol::TrajectoryAvailability::Unavailable
        }
        TrajectoryAvailability::Error => wakuwaku_protocol::TrajectoryAvailability::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use wakuwaku_harness::{TextBlock, ThinkingBlock, ToolCall, Usage};

    fn assistant(text: &str) -> Arc<AssistantMessage> {
        Arc::new(AssistantMessage {
            content: vec![ContentBlock::Text(TextBlock {
                text: text.into(),
                signature: Some("sig-secret".into()),
            })],
            model: "m".into(),
            provider: "p".into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
        })
    }

    fn kinds(batch: &TrajectoryBatch) -> Vec<TrajectoryKind> {
        batch
            .ops
            .iter()
            .filter_map(|op| match op {
                TrajectoryOp::UpsertRecord(record) => Some(record.kind),
                TrajectoryOp::UpsertPrompt(_) => None,
            })
            .collect()
    }

    #[test]
    fn record_drive_projects_trace_kinds_without_token_rows() {
        let start = Instant::now();
        let first = start + Duration::from_millis(12);
        let tool_end = start + Duration::from_millis(40);
        let batch = record_drive(
            Uuid::nil(),
            Some(TrajectoryUserInput {
                text: "hello".into(),
                display_text: Some("hello".into()),
                ..TrajectoryUserInput::default()
            }),
            Vec::new(),
            vec![
                TraceEvent::PromptPrepared {
                    system_prompt: Some("sys".into()),
                    tools_json: Arc::from("[]"),
                    options_json: Arc::from("{}"),
                    model_hint: "model".into(),
                },
                TraceEvent::RequestStart {
                    visible_turn: 1,
                    step: 1,
                    started_at: start,
                },
                TraceEvent::RequestFirstToken {
                    visible_turn: 1,
                    step: 1,
                    at: first,
                },
                TraceEvent::ToolExecution {
                    call_id: "call-1".into(),
                    started_at: start,
                    finished_at: tool_end,
                    result_preview: Arc::from("ok"),
                },
                TraceEvent::AssistantDone(assistant("done")),
            ],
        );
        assert_eq!(
            kinds(&batch),
            vec![
                TrajectoryKind::System,
                TrajectoryKind::User,
                TrajectoryKind::Request,
                TrajectoryKind::Request,
                TrajectoryKind::Tool,
                TrajectoryKind::Assistant,
                TrajectoryKind::Request,
            ]
        );
        let request = batch.ops.iter().rev().find_map(|op| match op {
            TrajectoryOp::UpsertRecord(record) if record.kind == TrajectoryKind::Request => {
                Some(record)
            }
            _ => None,
        });
        let request = request.expect("completed request");
        assert!(request.ttft_ms.is_some());
        assert!(request.duration_ms.is_some());
        let assistant_detail = batch
            .ops
            .iter()
            .find_map(|op| match op {
                TrajectoryOp::UpsertRecord(record) if record.kind == TrajectoryKind::Assistant => {
                    Some(record.detail_json.as_str())
                }
                _ => None,
            })
            .unwrap();
        assert!(!assistant_detail.contains("sig-secret"));
    }

    #[test]
    fn record_drive_does_not_infer_missing_timing() {
        let batch = record_drive(
            Uuid::nil(),
            Some(TrajectoryUserInput {
                text: "hi".into(),
                ..TrajectoryUserInput::default()
            }),
            Vec::new(),
            vec![TraceEvent::SteeringInjected { id: 7 }],
        );
        let context = batch.ops.iter().find_map(|op| match op {
            TrajectoryOp::UpsertRecord(record) if record.kind == TrajectoryKind::Context => {
                Some(record)
            }
            _ => None,
        });
        let context = context.expect("context");
        assert!(context.ttft_ms.is_none());
        assert!(context.duration_ms.is_none());
    }

    #[test]
    fn snapshot_projection_leaves_timing_empty() {
        let snapshot = wakuwaku_harness::Session::with_messages(
            Some("sys".into()),
            vec![
                Message::User(UserMessage::text("hi")),
                Message::Assistant(assistant("yo")),
            ],
        )
        .snapshot();
        let batch = project_snapshot(Uuid::nil(), &snapshot);
        assert!(batch.ops.iter().any(|op| matches!(
            op,
            TrajectoryOp::UpsertRecord(record) if record.kind == TrajectoryKind::System
        )));
        for op in &batch.ops {
            if let TrajectoryOp::UpsertRecord(record) = op {
                assert!(record.started_at_ms.is_none());
                assert!(record.duration_ms.is_none());
                assert!(record.ttft_ms.is_none());
            }
        }
    }

    #[test]
    fn legacy_session_projection_does_not_use_turn_seconds() {
        let mut session = AgentSession::new(
            Uuid::nil(),
            wakuwaku_protocol::ProviderId::new(wakuwaku_protocol::ProviderId::OPENAI_RESPONSES),
        );
        session.begin_turn("hello");
        session.push_message(MessageRole::Assistant, "world");
        if let Some(turn) = session.turns.last_mut() {
            turn.started_at = 100;
            turn.completed_at = Some(140);
        }
        let batch = project_legacy_session(session.id, &session);
        for op in &batch.ops {
            if let TrajectoryOp::UpsertRecord(record) = op {
                assert!(
                    record.duration_ms.is_none(),
                    "legacy must not infer duration"
                );
                assert!(record.ttft_ms.is_none());
            }
        }
    }

    #[test]
    fn thinking_signatures_stay_out_of_detail() {
        let message = Arc::new(AssistantMessage {
            content: vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: "plan".into(),
                signature: Some("anth-sig".into()),
                redacted: false,
            })],
            model: "m".into(),
            provider: "p".into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
        });
        let detail = assistant_detail(&message).to_string();
        assert!(detail.contains("plan"));
        assert!(!detail.contains("anth-sig"));
        let _ = ToolCall {
            id: "x".into(),
            name: "n".into(),
            arguments: json!({}),
            thought_signature: Some("nope".into()),
        };
    }
}
