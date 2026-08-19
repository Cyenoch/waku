use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

use crate::attachments::{AttachmentUpload, PromptInput, StoredAttachment};
use crate::model::{
    AgentSession, AuthPhase, BackgroundWorkKey, LoginMethod, ModelCatalog, Project,
    ProviderAuthStatus, ProviderId, SecretString, ServiceTier, UserInputAnswer,
};
use crate::persistence::{ComposerDraftChange, ComposerDrafts, SessionMessageMatch};
use crate::settings::DaemonSettings;
use crate::skills::SkillsCatalog;
use crate::trajectory::{TrajectoryLiveUpdate, TrajectoryQuery, TrajectoryResponse};
use crate::usage_history::{UsageHistory, UsageWindow};
use crate::workspace::{WorkspaceOperation, WorkspaceResult};

pub const PROTOCOL_VERSION: u32 = 5;
pub const MAX_WIRE_MESSAGE_BYTES: usize = 48 * 1024 * 1024;
pub const DAEMON_TOKEN_ENV: &str = "WAKUWAKU_DAEMON_TOKEN";
pub const DAEMON_ADDRESS_ENV: &str = "WAKUWAKU_DAEMON_ADDRESS";
pub const APP_EXECUTABLE_ENV: &str = "WAKUWAKU_APP_EXECUTABLE";

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DaemonReady {
    pub address: String,
    pub protocol_version: u32,
    pub pid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        token: String,
        client_id: Uuid,
        #[serde(default)]
        resume_from: Vec<ReplayCursor>,
    },
    Request(Request),
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCursor {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Identifies the daemon process that assigned `sequence`.
    pub epoch: Uuid,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    /// Resolve the daemon-owned provider runtime for an existing task.
    ///
    /// Clients use this after reconnecting or opening the same daemon from a
    /// second app. It observes the session actor without starting, replacing,
    /// or otherwise mutating the provider process.
    AttachSession,
    Start {
        options: WireDriverStartOptions,
    },
    Prompt {
        input: PromptInput,
    },
    Steer {
        input: PromptInput,
    },
    Cancel,
    Respond {
        request_id: String,
        option_id: String,
    },
    RespondUserInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    RefreshBackgroundWork,
    StopBackgroundWork {
        key: BackgroundWorkKey,
        control_id: String,
    },
    ApplyOptions {
        options: WireSessionOptions,
    },
    GetSettings,
    UpdateSettings {
        settings: DaemonSettings,
    },
    LoadUsageHistory {
        window: UsageWindow,
    },
    LoadSkills {
        projects: Vec<(String, PathBuf)>,
    },
    SetSkillsEnabled {
        dirs: Vec<PathBuf>,
        enabled: bool,
    },
    TrashSkills {
        dirs: Vec<PathBuf>,
    },
    LoadTaskState,
    SaveTaskState(Box<SaveTaskState>),
    /// Explicitly remove one daemon-owned task. Ordinary state saves are
    /// merge-only so a stale client snapshot cannot delete tasks another
    /// client just created.
    RemoveSession,
    HydrateSession {
        session_id: Uuid,
    },
    SearchSessionMessages {
        query: String,
        limit: usize,
    },
    LoadComposerDrafts,
    SaveComposerDrafts {
        drafts: ComposerDrafts,
        generation: u64,
    },
    ApplyComposerDraftChanges {
        changes: Vec<ComposerDraftChange>,
    },
    StoreBlob {
        mime_type: String,
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    ImportAttachment {
        name: String,
        upload: AttachmentUpload,
    },
    ImportPathAttachment {
        #[ts(type = "string")]
        path: PathBuf,
    },
    ReadBlob {
        reference: String,
    },
    ReadAttachment {
        reference: String,
        path: PathBuf,
    },
    SweepBlobs,
    /// Fork a persisted task through one completed provider turn.
    ///
    /// This is intentionally a daemon-owned operation: provider-native
    /// conversation state, Git checkpoint refs, and SQLite all live on the
    /// daemon host and must move together for remote clients.
    ForkSessionFromResponse {
        turn_count: usize,
    },
    /// Restore a task and its provider conversation to immediately before a
    /// prior user message. The client can then submit the edited replacement
    /// as an ordinary new turn.
    RewindSessionToMessage {
        turn_count: usize,
    },
    Workspace {
        operation: WorkspaceOperation,
    },
    OpenTerminal {
        #[ts(type = "string")]
        cwd: PathBuf,
        cols: u16,
        rows: u16,
    },
    WriteTerminal {
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        data: Vec<u8>,
    },
    ResizeTerminal {
        cols: u16,
        rows: u16,
    },
    CloseTerminal,
    CloseSession,
    GetAuthStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<ProviderId>,
    },
    StartLogin {
        provider: ProviderId,
        method: LoginMethod,
    },
    CompleteApiKeyLogin {
        login_id: Uuid,
        provider: ProviderId,
        key: SecretString,
    },
    CancelLogin {
        login_id: Uuid,
    },
    Logout {
        provider: ProviderId,
    },
    ListModels {
        provider: ProviderId,
    },
    RefreshModels {
        provider: ProviderId,
    },
    QueryTrajectory {
        query: TrajectoryQuery,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverStartOptions {
    pub provider: ProviderId,
    pub cwd: PathBuf,
    pub mode: String,
    pub interaction_mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    pub context_window: Option<String>,
    /// Locally persisted task to create or restore before the embedded driver
    /// starts. Absent for already-daemon-owned tasks. Unknown ids without this
    /// payload stay unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<Box<StartTask>>,
}

/// Client-owned task accepted for submit. The daemon upserts this before
/// reconstructing an embedded transcript so a relaunch or fresh database cannot
/// lose a session the app already persisted.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StartTask {
    pub session: AgentSession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Project>,
    /// Fingerprint of the client's provider-started transcript. Unstarted
    /// user turns are excluded. Stale restores do not replace a newer
    /// daemon projection; Start still proceeds against the canonical task.
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireSessionOptions {
    pub mode: String,
    pub interaction_mode: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    pub context_window: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WireDriverEvent {
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

impl WireDriverEvent {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SequencedPayload {
    Driver { event: WireDriverEvent },
    Trajectory { update: TrajectoryLiveUpdate },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SequencedEvent {
    pub session_id: Uuid,
    pub runtime_id: Uuid,
    /// Changes whenever the daemon restarts, so a reused runtime id can begin
    /// again at sequence one without being mistaken for an old event.
    pub epoch: Uuid,
    pub sequence: u64,
    pub payload: SequencedPayload,
}

impl SequencedEvent {
    pub fn driver(&self) -> Option<&WireDriverEvent> {
        match &self.payload {
            SequencedPayload::Driver { event } => Some(event),
            SequencedPayload::Trajectory { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Hello {
        protocol_version: u32,
        daemon_version: String,
    },
    Rejected {
        message: String,
    },
    Response {
        request_id: Uuid,
        outcome: ResponseOutcome,
    },
    Event(Box<SequencedEvent>),
    /// The daemon-owned project/task catalog changed through another client.
    /// Clients should invalidate their lightweight task-state snapshot; live
    /// runtime events continue through [`Self::Event`].
    TaskStateChanged {
        revision: u64,
    },
    ShuttingDown,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponseOutcome {
    Ok { payload: Box<ResponsePayload> },
    Error { error: RpcError },
}

impl ResponseOutcome {
    pub fn ok(payload: ResponsePayload) -> Self {
        Self::Ok {
            payload: Box::new(payload),
        }
    }

    pub fn payload(&self) -> Option<&ResponsePayload> {
        match self {
            Self::Ok { payload } => Some(payload.as_ref()),
            Self::Error { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ResponsePayload {
    Ack,
    SessionRuntime {
        runtime_id: Option<Uuid>,
        supports_steer: bool,
    },
    Started {
        supports_steer: bool,
        /// Generation the daemon accepted when Start carried a task payload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_generation: Option<u64>,
    },
    OptionsApplied {
        applied: bool,
    },
    Settings {
        settings: DaemonSettings,
    },
    UsageHistory {
        history: UsageHistory,
    },
    SkillsCatalog {
        catalog: SkillsCatalog,
    },
    TaskState {
        projects: Vec<Project>,
        sessions: Vec<AgentSession>,
        default_cwd: PathBuf,
        projectless_root: Option<PathBuf>,
    },
    TaskStateSaved {
        sessions: Vec<AgentSession>,
    },
    Session {
        session: Option<Box<AgentSession>>,
    },
    SessionMessageMatches {
        matches: Vec<SessionMessageMatch>,
    },
    ComposerDrafts {
        drafts: ComposerDrafts,
    },
    BlobStored {
        reference: String,
        path: PathBuf,
    },
    AttachmentStored {
        attachment: StoredAttachment,
    },
    BlobData {
        #[serde(with = "base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    SessionForked {
        session: Box<AgentSession>,
        checkpoint_warning: Option<String>,
    },
    SessionRewound {
        session: Box<AgentSession>,
        cleanup_warning: Option<String>,
    },
    Workspace {
        result: WorkspaceResult,
    },
    AuthStatus {
        statuses: Vec<ProviderAuthStatus>,
        #[serde(default)]
        phases: Vec<AuthPhase>,
    },
    Login {
        phase: AuthPhase,
    },
    Models {
        catalog: ModelCatalog,
    },
    Trajectory {
        response: Box<TrajectoryResponse>,
    },
}

/// Boxed `Command::SaveTaskState` payload. Serde still flattens these fields
/// onto the command object, so the wire JSON is unchanged.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SaveTaskState {
    pub projects: Vec<Project>,
    pub live_session_ids: Vec<Uuid>,
    pub sessions: Vec<AgentSession>,
}

impl SaveTaskState {
    pub fn boxed(
        projects: Vec<Project>,
        live_session_ids: Vec<Uuid>,
        sessions: Vec<AgentSession>,
    ) -> Box<Self> {
        Box::new(Self {
            projects,
            live_session_ids,
            sessions,
        })
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
pub struct RpcError {
    pub message: String,
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_payloads_use_base64_json_strings() {
        let payload = ResponsePayload::BlobData {
            bytes: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["bytes"], "AAEC/w==");
        let ResponsePayload::BlobData { bytes } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected payload variant");
        };
        assert_eq!(bytes, vec![0, 1, 2, 255]);

        let command = Command::WriteTerminal {
            data: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["type"], "writeTerminal");
        assert_eq!(json["data"], "AAEC/w==");
        let Command::WriteTerminal { data } = serde_json::from_value(json).unwrap() else {
            panic!("unexpected command variant");
        };
        assert_eq!(data, vec![0, 1, 2, 255]);
    }

    #[test]
    fn response_fork_command_uses_stable_camel_case_fields() {
        let json =
            serde_json::to_value(Command::ForkSessionFromResponse { turn_count: 7 }).unwrap();

        assert_eq!(json["type"], "forkSessionFromResponse");
        assert_eq!(json["turnCount"], 7);
        assert_eq!(PROTOCOL_VERSION, 5);
    }

    #[test]
    fn message_rewind_command_uses_stable_camel_case_fields() {
        let json = serde_json::to_value(Command::RewindSessionToMessage { turn_count: 4 }).unwrap();

        assert_eq!(json["type"], "rewindSessionToMessage");
        assert_eq!(json["turnCount"], 4);
        assert_eq!(PROTOCOL_VERSION, 5);
    }

    #[test]
    fn handshake_and_replay_field_names_are_stable() {
        let session_id = Uuid::nil();
        let runtime_id = Uuid::from_u128(1);
        let message = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: "secret".into(),
            client_id: Uuid::from_u128(2),
            resume_from: vec![ReplayCursor {
                session_id,
                runtime_id,
                epoch: Uuid::from_u128(3),
                sequence: 9,
            }],
        };
        let json = serde_json::to_value(message).unwrap();

        assert_eq!(json["type"], "hello");
        assert_eq!(json["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["resumeFrom"][0]["sessionId"], session_id.to_string());
        assert_eq!(json["resumeFrom"][0]["runtimeId"], runtime_id.to_string());
        assert_eq!(
            json["resumeFrom"][0]["epoch"],
            Uuid::from_u128(3).to_string()
        );
        assert!(json.get("protocol_version").is_none());
    }

    #[test]
    fn composer_draft_changes_have_stable_wire_keys() {
        let project_id = Uuid::from_u128(7);
        let command = Command::ApplyComposerDraftChanges {
            changes: vec![ComposerDraftChange {
                target: crate::persistence::ComposerDraftTarget::NewSession { project_id },
                draft: Some(crate::persistence::ComposerDraft {
                    text: "unfinished".into(),
                    attachments: Vec::new(),
                }),
            }],
        };
        let json = serde_json::to_value(command).unwrap();

        assert_eq!(json["type"], "applyComposerDraftChanges");
        assert_eq!(json["changes"][0]["target"]["type"], "newSession");
        assert_eq!(
            json["changes"][0]["target"]["projectId"],
            project_id.to_string()
        );
        assert_eq!(json["changes"][0]["draft"]["text"], "unfinished");
    }

    #[test]
    fn complete_api_key_login_debug_redacts_the_secret() {
        let command = Command::CompleteApiKeyLogin {
            login_id: Uuid::nil(),
            provider: ProviderId::new("xai"),
            key: crate::SecretString::new("sk-never-log-this"),
        };
        let rendered = format!("{command:?}");
        assert!(!rendered.contains("sk-never-log-this"));
        assert!(rendered.contains("redacted"));
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["type"], "completeApiKeyLogin");
        assert_eq!(json["provider"], "xai");
    }

    #[test]
    fn save_task_state_keeps_flattened_camel_case_fields() {
        let json = serde_json::to_value(Command::SaveTaskState(SaveTaskState::boxed(
            Vec::new(),
            vec![Uuid::nil()],
            Vec::new(),
        )))
        .unwrap();
        assert_eq!(json["type"], "saveTaskState");
        assert_eq!(json["liveSessionIds"][0], Uuid::nil().to_string());
        assert!(json["sessions"].is_array());
        assert!(json.get("0").is_none());
    }

    #[test]
    fn start_task_restore_uses_stable_camel_case_fields() {
        let session = AgentSession::new(Uuid::from_u128(1), ProviderId::new("openai-responses"));
        let json = serde_json::to_value(Command::Start {
            options: WireDriverStartOptions {
                provider: ProviderId::new("openai-responses"),
                cwd: PathBuf::from("/tmp"),
                mode: "ask".into(),
                interaction_mode: "build".into(),
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                task: Some(Box::new(StartTask {
                    session,
                    project: None,
                    generation: 9,
                })),
            },
        })
        .unwrap();
        assert_eq!(json["type"], "start");
        assert_eq!(json["options"]["task"]["generation"], 9);
        assert!(json["options"]["task"]["session"]["id"].is_string());
        assert!(json["options"].get("taskId").is_none());
    }

    #[test]
    fn boxed_response_outcome_keeps_nested_payload_object() {
        let json = serde_json::to_value(ResponseOutcome::ok(ResponsePayload::Ack)).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["payload"]["type"], "ack");
        assert!(json["payload"].is_object());
    }

    #[test]
    fn auth_and_model_commands_use_stable_camel_case() {
        let json = serde_json::to_value(Command::StartLogin {
            provider: ProviderId::new("openai-codex"),
            method: crate::LoginMethod::OauthBrowser,
        })
        .unwrap();
        assert_eq!(json["type"], "startLogin");
        assert_eq!(json["method"], "oauthBrowser");
        let json = serde_json::to_value(ResponsePayload::Models {
            catalog: crate::ModelCatalog {
                provider: ProviderId::new("opencode-go"),
                models: Vec::new(),
                source: crate::CatalogSource::Live,
                fetched_at_ms: 1,
            },
        })
        .unwrap();
        assert_eq!(json["type"], "models");
        assert_eq!(json["catalog"]["source"], "live");
    }

    #[test]
    fn auth_status_phases_carry_login_id_and_provider() {
        let phase = crate::AuthPhase::AwaitingApiKey {
            login_id: Uuid::nil(),
            provider: ProviderId::new("xai"),
            instructions: "Paste the API key for xai".into(),
        };
        let json = serde_json::to_value(ResponsePayload::AuthStatus {
            statuses: Vec::new(),
            phases: vec![phase],
        })
        .unwrap();
        assert_eq!(json["type"], "authStatus");
        assert_eq!(json["phases"][0]["type"], "awaitingApiKey");
        assert_eq!(json["phases"][0]["loginId"], Uuid::nil().to_string());
        assert_eq!(json["phases"][0]["provider"], "xai");
    }

    #[test]
    fn sequenced_payload_is_a_camel_case_tagged_union() {
        let driver = SequencedEvent {
            session_id: Uuid::from_u128(1),
            runtime_id: Uuid::from_u128(2),
            epoch: Uuid::from_u128(3),
            sequence: 4,
            payload: SequencedPayload::Driver {
                event: WireDriverEvent::new("textDelta", serde_json::json!("hi")),
            },
        };
        let json = serde_json::to_value(&driver).unwrap();
        assert_eq!(json["payload"]["type"], "driver");
        assert_eq!(json["payload"]["event"]["kind"], "textDelta");
        assert!(json.get("event").is_none());
        let back: SequencedEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back.driver().unwrap().kind, "textDelta");

        let live = SequencedPayload::Trajectory {
            update: crate::TrajectoryLiveUpdate::Reset {
                generation: 2,
                revision: 7,
            },
        };
        let json = serde_json::to_value(&live).unwrap();
        assert_eq!(json["type"], "trajectory");
        assert_eq!(json["update"]["type"], "reset");
        assert_eq!(json["update"]["revision"], 7);
    }

    #[test]
    fn query_trajectory_command_and_response_are_tagged() {
        let command = Command::QueryTrajectory {
            query: crate::TrajectoryQuery::Page {
                before: None,
                limit: Some(25),
                at_least_revision: Some(3),
            },
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["type"], "queryTrajectory");
        assert_eq!(json["query"]["type"], "page");
        assert_eq!(json["query"]["limit"], 25);
        let payload = ResponsePayload::Trajectory {
            response: Box::new(crate::TrajectoryResponse::Page {
                availability: crate::TrajectoryAvailability::Exact,
                generation: 1,
                revision: 3,
                rows: Vec::new(),
                older: None,
                newer: None,
                has_older: false,
                has_newer: false,
            }),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "trajectory");
        assert_eq!(json["response"]["type"], "page");
        assert_eq!(json["response"]["revision"], 3);
    }
}
