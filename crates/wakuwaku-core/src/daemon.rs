//! Provider backend and driver-event wire translation for `wakuwaku-daemon`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::{
    Backend, Command, EventHub, EventSink, Request, ResponsePayload, StartTask, WireDriverEvent,
    WorkspaceOperation, WorkspaceResult,
};
use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;
use wakuwaku_protocol::{decode_enum, event_to_wire};

use crate::attachments::AttachmentStore;
use crate::auth::{AuthRuntime, AuthService};
use crate::driver::{self, DriverHandle, DriverStartOptions, SessionOptions};
use crate::model::{
    AgentSession, Checkpoint, CheckpointStatus, DriverEvent, Project, SessionStatus, TurnStatus,
    unix_time,
};
use crate::persistence::{ComposerDraftStore, PersistedState, StateStore};
use crate::settings::DaemonSettingsStore;
use crate::trajectory::{TraceHandoff, TrajectoryInitSource, TrajectoryUserInput};
use crate::trajectory_store::TrajectoryWriter;

type CheckpointLocks = Mutex<HashMap<(PathBuf, Uuid, usize), Arc<Mutex<()>>>>;

pub struct WakuBackend {
    sessions: Mutex<HashMap<Uuid, (Uuid, DriverHandle)>>,
    terminals: Mutex<HashMap<Uuid, (Uuid, crate::terminal::DaemonTerminal)>>,
    settings: DaemonSettingsStore,
    auth: AuthService,
    task_store: Arc<StateStore>,
    task_state: Arc<Mutex<PersistedState>>,
    removed_session_ids: Mutex<HashSet<Uuid>>,
    composer_drafts: ComposerDraftStore,
    attachments: AttachmentStore,
    checkpoint_capture_locks: CheckpointLocks,
    usage_rates_dir: std::path::PathBuf,
    default_cwd: std::path::PathBuf,
    trajectory: Arc<TrajectoryWriter>,
    trace_handoff: Arc<TraceHandoff>,
    live_hub: Arc<Mutex<Option<EventHub>>>,
    session_event_log: bool,
}

impl WakuBackend {
    pub fn new(settings: DaemonSettingsStore, task_store: StateStore) -> anyhow::Result<Self> {
        let directory = task_store
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned();
        let auth = AuthService::new(AuthRuntime::production(&directory)?)
            .map_err(|error| anyhow!(error))?;
        Self::new_with_auth(settings, task_store, auth)
    }

    pub fn new_with_auth(
        settings: DaemonSettingsStore,
        task_store: StateStore,
        auth: AuthService,
    ) -> anyhow::Result<Self> {
        let session_event_log =
            std::env::var("WAKUWAKU_SESSION_EVENT_LOG").is_ok_and(|value| value == "1");
        Self::new_with_auth_and_session_event_log(settings, task_store, auth, session_event_log)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_auth_and_session_event_log(
        settings: DaemonSettingsStore,
        task_store: StateStore,
        auth: AuthService,
        session_event_log: bool,
    ) -> anyhow::Result<Self> {
        let task_state = task_store
            .load()
            .context("could not load WakuWaku task database")?;
        let composer_drafts = ComposerDraftStore::for_state_path(task_store.path());
        let attachments = AttachmentStore::new(
            task_store
                .path()
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("attachments"),
        );
        let usage_rates_dir = task_store
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned();
        let live_hub = Arc::new(Mutex::new(None));
        let trajectory = Arc::new(
            TrajectoryWriter::open_with_live(task_store.path(), {
                let live_hub = Arc::clone(&live_hub);
                move |update| {
                    let hub: Option<EventHub> = live_hub.lock().clone();
                    let Some(hub) = hub else {
                        return;
                    };
                    for wire in crate::trajectory::to_wire_live_updates(&update) {
                        hub.emit_trajectory(update.session_id, wire);
                    }
                }
            })
            .map_err(|error| anyhow!(error))?,
        );
        let trace_handoff = Arc::new(TraceHandoff::spawn(Arc::clone(&trajectory) as Arc<_>));
        Ok(Self {
            sessions: Mutex::new(HashMap::new()),
            terminals: Mutex::new(HashMap::new()),
            settings,
            auth,
            task_store: Arc::new(task_store),
            task_state: Arc::new(Mutex::new(task_state)),
            removed_session_ids: Mutex::new(HashSet::new()),
            composer_drafts,
            attachments,
            checkpoint_capture_locks: Mutex::new(HashMap::new()),
            usage_rates_dir,
            default_cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            trajectory,
            trace_handoff,
            live_hub,
            session_event_log,
        })
    }

    /// Enqueues one shadow session event. Diagnostics only: never blocks
    /// wire delivery and never fails the turn.
    fn observe_session_event(
        &self,
        session_id: Uuid,
        runtime_id: Option<Uuid>,
        turn_id: Option<Uuid>,
        payload: crate::session_events::SessionEventPayload,
    ) {
        if !self.session_event_log {
            return;
        }
        let event = crate::session_events::NewSessionEvent::observed(runtime_id, turn_id, payload);
        match self
            .trajectory
            .try_append_session_events(session_id, vec![event])
        {
            crate::trajectory_store::SessionEventEnqueue::Queued => {}
            crate::trajectory_store::SessionEventEnqueue::Full => {
                report_shadow_enqueue_failure("writer queue full");
            }
            crate::trajectory_store::SessionEventEnqueue::Disconnected => {
                report_shadow_enqueue_failure("writer disconnected");
            }
        }
    }

    /// Capture and persist one ending checkpoint exactly once per daemon.
    /// Desktop and Web may observe the same turn completion concurrently; a
    /// per-turn lock prevents both clients from running the expensive Git
    /// snapshot while leaving unrelated tasks independent.
    fn capture_turn_checkpoint(
        &self,
        cwd: PathBuf,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<Checkpoint> {
        let key = (cwd.clone(), session_id, turn_count);
        let capture_lock = self
            .checkpoint_capture_locks
            .lock()
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _capture = capture_lock.lock();

        {
            let mut state = self.task_state.lock();
            if let Some(index) = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
            {
                self.task_store.hydrate(&mut state.sessions[index])?;
                if let Some(checkpoint) = state.sessions[index]
                    .turns
                    .iter()
                    .find(|turn| turn.turn_count == turn_count)
                    .and_then(|turn| turn.checkpoint.as_ref())
                    .filter(|checkpoint| {
                        matches!(
                            checkpoint.status,
                            CheckpointStatus::Ready | CheckpointStatus::Unavailable
                        )
                    })
                {
                    return Ok(checkpoint.clone());
                }
            }
        }

        let checkpoint = crate::checkpoint::capture_turn(&cwd, session_id, turn_count)?;
        let mut state = self.task_state.lock();
        if let Some(index) = state
            .sessions
            .iter()
            .position(|session| session.id == session_id)
        {
            self.task_store.hydrate(&mut state.sessions[index])?;
            if let Some(turn) = state.sessions[index]
                .turns
                .iter_mut()
                .find(|turn| turn.turn_count == turn_count)
            {
                turn.checkpoint = Some(checkpoint.clone());
                state.mark_session_dirty(session_id);
                self.task_store.save(&mut state)?;
            }
        }
        Ok(checkpoint)
    }
}

impl Backend for WakuBackend {
    fn handle(&self, request: Request, events: EventSink) -> anyhow::Result<ResponsePayload> {
        let session_id = request.session_id;
        let runtime_id = request.runtime_id;
        match request.command {
            Command::AttachSession => {
                let sessions = self.sessions.lock();
                let Some((runtime_id, driver)) = sessions.get(&session_id) else {
                    return Ok(ResponsePayload::SessionRuntime {
                        runtime_id: None,
                        supports_steer: false,
                    });
                };
                Ok(ResponsePayload::SessionRuntime {
                    runtime_id: Some(*runtime_id),
                    supports_steer: driver.supports_steer(),
                })
            }
            Command::GetSettings => Ok(ResponsePayload::Settings {
                settings: self.settings.get(),
            }),
            Command::UpdateSettings { settings } => {
                self.settings.replace(settings)?;
                Ok(ResponsePayload::Ack)
            }
            Command::GetAuthStatus { provider } => {
                self.auth
                    .set_custom_providers(self.settings.get().external_providers);
                Ok(ResponsePayload::AuthStatus {
                    statuses: self.auth.status(provider.as_ref()),
                    phases: self.auth.auth_phases(provider.as_ref()),
                })
            }
            Command::StartLogin { provider, method } => Ok(ResponsePayload::Login {
                phase: self.auth.start_login(provider, method)?,
            }),
            Command::CompleteApiKeyLogin {
                login_id,
                provider,
                key,
            } => Ok(ResponsePayload::Login {
                phase: self.auth.complete_api_key(login_id, provider, key)?,
            }),
            Command::CancelLogin { login_id } => {
                self.auth.cancel_login(login_id)?;
                Ok(ResponsePayload::Ack)
            }
            Command::Logout { provider } => {
                self.auth.logout(&provider)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ListModels { provider } => {
                self.auth
                    .set_custom_providers(self.settings.get().external_providers);
                Ok(ResponsePayload::Models {
                    catalog: self.auth.list_models(&provider)?,
                })
            }
            Command::RefreshModels { provider } => {
                self.auth
                    .set_custom_providers(self.settings.get().external_providers);
                Ok(ResponsePayload::Models {
                    catalog: self.auth.refresh_models(&provider)?,
                })
            }
            Command::LoadUsageHistory { window } => {
                let rates = crate::usage_history::load_rate_table(&self.usage_rates_dir);
                let today = chrono::Local::now().date_naive();
                let (since_ms, until_ms) = crate::usage_history::window_millis(window, today);
                let events = self.task_store.usage_events_between(since_ms, until_ms)?;
                let history = crate::usage_history::fold(&rates, window, &events);
                Ok(ResponsePayload::UsageHistory { history })
            }
            Command::LoadSkills { projects } => {
                let locations = crate::skills::skill_locations(&projects);
                Ok(ResponsePayload::SkillsCatalog {
                    catalog: crate::skills::scan_skills(&locations),
                })
            }
            Command::SetSkillsEnabled { dirs, enabled } => {
                for dir in dirs {
                    crate::skills::set_skill_enabled(&dir, enabled)
                        .map_err(|error| anyhow!(error))?;
                }
                Ok(ResponsePayload::Ack)
            }
            Command::TrashSkills { dirs } => {
                crate::skills::trash_skills(&dirs).map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Ack)
            }
            Command::LoadTaskState => {
                let state = self.task_state.lock();
                Ok(ResponsePayload::TaskState {
                    projects: state.projects.clone(),
                    sessions: state
                        .sessions
                        .iter()
                        .map(AgentSession::list_projection)
                        .collect(),
                    default_cwd: self.default_cwd.clone(),
                    projectless_root: crate::projectless::workspace_root(),
                })
            }
            Command::SaveTaskState(payload) => {
                let wakuwaku_protocol::SaveTaskState {
                    projects,
                    live_session_ids: _,
                    sessions,
                } = *payload;
                let active_runtimes = self
                    .sessions
                    .lock()
                    .iter()
                    .map(|(session_id, (runtime_id, _))| (*session_id, *runtime_id))
                    .collect::<HashMap<_, _>>();
                let mut state = self.task_state.lock();
                let removed_session_ids = self.removed_session_ids.lock();
                for project in projects {
                    if let Some(existing) = state
                        .projects
                        .iter_mut()
                        .find(|existing| existing.id == project.id)
                    {
                        *existing = project;
                    } else {
                        state.projects.push(project);
                    }
                }
                let sessions = sessions
                    .into_iter()
                    .filter(|session| !removed_session_ids.contains(&session.id))
                    .collect::<Vec<_>>();
                drop(removed_session_ids);
                let saved_ids = sessions
                    .iter()
                    .map(|session| session.id)
                    .collect::<Vec<_>>();
                for mut session in sessions {
                    if let Some(existing) = state
                        .sessions
                        .iter_mut()
                        .find(|existing| existing.id == session.id)
                    {
                        if session_projection_precedes(
                            existing,
                            &session,
                            active_runtimes.get(&session.id).copied(),
                        ) {
                            merge_stale_session_metadata(existing, session);
                        } else {
                            preserve_daemon_checkpoints(existing, &mut session);
                            *existing = session;
                        }
                    } else {
                        state.sessions.push(session);
                    }
                }
                let used_project_ids = state
                    .sessions
                    .iter()
                    .map(|session| session.project_id)
                    .collect::<std::collections::HashSet<_>>();
                state.projects.retain(|project| {
                    !project.is_projectless() || used_project_ids.contains(&project.id)
                });
                for session_id in &saved_ids {
                    state.mark_session_dirty(*session_id);
                }
                self.task_store.save(&mut state)?;
                let sessions = saved_ids
                    .into_iter()
                    .filter_map(|session_id| {
                        state
                            .sessions
                            .iter()
                            .find(|session| session.id == session_id)
                            .cloned()
                    })
                    .collect();
                Ok(ResponsePayload::TaskStateSaved { sessions })
            }
            Command::RemoveSession => {
                let removed = self.sessions.lock().remove(&session_id);
                if let Some((_, driver)) = removed.as_ref() {
                    driver.cancel();
                }
                drop(removed);
                self.trace_handoff.discard(session_id);
                let _ = self.trajectory.flush(session_id);
                {
                    let mut state = self.task_state.lock();
                    self.removed_session_ids.lock().insert(session_id);
                    let project_id = state
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| session.project_id);
                    state.sessions.retain(|session| session.id != session_id);
                    if let Some(project_id) = project_id {
                        let remove_project = state
                            .projects
                            .iter()
                            .find(|project| project.id == project_id)
                            .is_some_and(Project::is_projectless)
                            && !state
                                .sessions
                                .iter()
                                .any(|session| session.project_id == project_id);
                        if remove_project {
                            state.projects.retain(|project| project.id != project_id);
                        }
                    }
                    self.task_store.save(&mut state)?;
                }
                Ok(ResponsePayload::Ack)
            }
            Command::HydrateSession { session_id } => {
                let mut state = self.task_state.lock();
                let session = if let Some(session) = state
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    self.task_store.hydrate(session)?;
                    Some(session.clone())
                } else {
                    None
                };
                Ok(ResponsePayload::Session {
                    session: session.map(Box::new),
                })
            }
            Command::SearchSessionMessages { query, limit } => {
                let matches = self.task_store.session_message_search(query, limit)()?;
                Ok(ResponsePayload::SessionMessageMatches { matches })
            }
            Command::LoadComposerDrafts => Ok(ResponsePayload::ComposerDrafts {
                drafts: self.composer_drafts.load()?,
            }),
            Command::SaveComposerDrafts { drafts, generation } => {
                self.composer_drafts.save(drafts, generation)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ApplyComposerDraftChanges { changes } => {
                self.composer_drafts.apply_changes(changes)?;
                Ok(ResponsePayload::Ack)
            }
            Command::StoreBlob { mime_type, bytes } => {
                let reference = self
                    .task_store
                    .blobs()
                    .store_image_bytes(&mime_type, &bytes)?;
                let path = self
                    .task_store
                    .blobs()
                    .path_for(&reference)
                    .ok_or_else(|| anyhow!("stored blob has no daemon path"))?;
                Ok(ResponsePayload::BlobStored { reference, path })
            }
            Command::ImportAttachment { name, upload } => Ok(ResponsePayload::AttachmentStored {
                attachment: self.attachments.import(&name, upload)?,
            }),
            Command::ImportPathAttachment { path } => Ok(ResponsePayload::AttachmentStored {
                attachment: self.attachments.import_path(&path)?,
            }),
            Command::ReadBlob { reference } => {
                let path = self
                    .task_store
                    .blobs()
                    .path_for(&reference)
                    .ok_or_else(|| anyhow!("invalid blob reference"))?;
                Ok(ResponsePayload::BlobData {
                    bytes: std::fs::read(path)?,
                })
            }
            Command::ReadAttachment { reference, path } => Ok(ResponsePayload::BlobData {
                bytes: self.attachments.read_file(&reference, &path)?,
            }),
            Command::SweepBlobs => {
                self.task_store.blob_sweep()();
                Ok(ResponsePayload::Ack)
            }
            Command::ForkSessionFromResponse { turn_count } => {
                let (session, checkpoint_warning) =
                    self.fork_session_from_response(session_id, turn_count)?;
                Ok(ResponsePayload::SessionForked {
                    session: Box::new(session),
                    checkpoint_warning,
                })
            }
            Command::RewindSessionToMessage { turn_count } => {
                let (session, cleanup_warning) =
                    self.rewind_session_to_message(session_id, turn_count)?;
                Ok(ResponsePayload::SessionRewound {
                    session: Box::new(session),
                    cleanup_warning,
                })
            }
            Command::Workspace {
                operation:
                    WorkspaceOperation::CaptureTurn {
                        cwd,
                        session_id,
                        turn_count,
                    },
            } => Ok(ResponsePayload::Workspace {
                result: WorkspaceResult::Checkpoint {
                    checkpoint: self.capture_turn_checkpoint(cwd, session_id, turn_count)?,
                },
            }),
            Command::Workspace { operation } => Ok(ResponsePayload::Workspace {
                result: crate::workspace::execute(operation)?,
            }),
            Command::OpenTerminal { cwd, cols, rows } => {
                ensure_shell_environment();
                let terminal = crate::terminal::DaemonTerminal::open(&cwd, cols, rows, events)?;
                let previous = self
                    .terminals
                    .lock()
                    .insert(session_id, (runtime_id, terminal));
                drop(previous);
                Ok(ResponsePayload::Ack)
            }
            Command::WriteTerminal { data } => {
                let terminals = self.terminals.lock();
                let (active_runtime_id, terminal) = terminals
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("daemon terminal {session_id} is not running"))?;
                if *active_runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                    );
                }
                terminal.write(data)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ResizeTerminal { cols, rows } => {
                let terminals = self.terminals.lock();
                let (active_runtime_id, terminal) = terminals
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("daemon terminal {session_id} is not running"))?;
                if *active_runtime_id != runtime_id {
                    bail!(
                        "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                    );
                }
                terminal.resize(cols, rows);
                Ok(ResponsePayload::Ack)
            }
            Command::CloseTerminal => {
                let removed = {
                    let mut terminals = self.terminals.lock();
                    if let Some((active_runtime_id, _)) = terminals.get(&session_id)
                        && *active_runtime_id != runtime_id
                    {
                        bail!(
                            "daemon terminal {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                        );
                    }
                    terminals.remove(&session_id)
                };
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            Command::Start { options } => {
                let previous = self.sessions.lock().remove(&session_id);
                let previous_runtime = previous.as_ref().map(|(runtime_id, _)| *runtime_id);
                if let Some((_, driver)) = previous.as_ref() {
                    self.store_live_snapshot(session_id, driver);
                }
                drop(previous);
                let (snapshot, task_generation) = self.prepare_embedded_start(
                    session_id,
                    options.task.map(|task| *task),
                    previous_runtime,
                )?;
                let provider_id = options.provider.clone();
                let settings = self.settings.get();
                self.auth.set_custom_providers(settings.external_providers);
                let (provider, transport, auth, extra_auth_headers, capabilities, limits) = self
                    .auth
                    .overlay_for_model(&provider_id, options.model.as_deref())?;
                let reasoning_effort = self
                    .auth
                    .resolve_reasoning_effort(
                        &provider_id,
                        options.model.as_deref(),
                        options.reasoning_effort.as_deref(),
                    )
                    .map(|(_, provider_value)| provider_value);
                let service_tier = options.service_tier.filter(|_| capabilities.service_tier);
                let options = DriverStartOptions {
                    provider,
                    cwd: options.cwd,
                    mode: decode_enum(&options.mode)?,
                    interaction_mode: decode_enum(&options.interaction_mode)?,
                    model: options.model,
                    reasoning_effort,
                    service_tier,
                    context_window: options.context_window,
                    snapshot,
                    auth,
                    transport,
                    extra_auth_headers,
                    capabilities,
                    limits,
                };
                let (wake, _wake_events) = smol::channel::bounded(1);
                let (event_sender, event_receiver) = driver::event_channel(wake);
                let handle = driver::start_local(
                    provider_id,
                    options,
                    event_sender,
                    session_id,
                    Arc::clone(&self.trace_handoff),
                )?;
                let supports_steer = handle.supports_steer();
                let persist_driver = handle.clone();
                let persist_store = Arc::clone(&self.task_store);
                let persist_state = Arc::clone(&self.task_state);
                let persist_handoff = Arc::clone(&self.trace_handoff);
                let shadow = ShadowEventCapture {
                    enabled: self.session_event_log,
                    writer: Arc::clone(&self.trajectory),
                };
                let shadow_runtime_id = runtime_id;
                std::thread::Builder::new()
                    .name(format!("wakuwaku-daemon-events-{session_id}"))
                    .spawn(move || {
                        let mut deliver_events = true;
                        let send = |wire| events.send(wire).is_ok();
                        while let Ok(output) = event_receiver.recv() {
                            let event = match output {
                                driver::DriverOutput::Event(event) => event,
                                driver::DriverOutput::TurnStarted { snapshot, ack } => {
                                    match persist_store
                                        .persist_harness_snapshot(session_id, *snapshot)
                                    {
                                        Ok(()) => {
                                            let _ = ack.send(true);
                                            DriverEvent::TurnStarted
                                        }
                                        Err(error) => {
                                            // Discard before releasing the
                                            // worker: once it sees the failed
                                            // ack it can accept a retried
                                            // prompt whose staged input must
                                            // not be swept by this discard.
                                            persist_handoff.discard(session_id);
                                            let _ = ack.send(false);
                                            report_snapshot_persist_error(&error);
                                            persist_and_forward_driver_event(
                                                &persist_store,
                                                &persist_state,
                                                &persist_handoff,
                                                session_id,
                                                DriverEvent::Error(format!(
                                                    "could not persist the admitted prompt: {error}"
                                                )),
                                                || {},
                                                &mut deliver_events,
                                                send,
                                            );
                                            DriverEvent::TurnFinished {
                                                success: false,
                                                summary: Some(error.to_string()),
                                            }
                                        }
                                    }
                                }
                            };
                            shadow.observe(session_id, shadow_runtime_id, &event);
                            persist_and_forward_driver_event(
                                &persist_store,
                                &persist_state,
                                &persist_handoff,
                                session_id,
                                event,
                                || {
                                    persist_driver_snapshot(
                                        &persist_store,
                                        session_id,
                                        &persist_driver,
                                    )
                                },
                                &mut deliver_events,
                                send,
                            );
                        }
                    })
                    .context("could not start daemon event forwarding thread")?;
                self.sessions
                    .lock()
                    .insert(session_id, (runtime_id, handle));
                Ok(ResponsePayload::Started {
                    supports_steer,
                    task_generation,
                })
            }
            Command::CloseSession => {
                let removed = {
                    let mut sessions = self.sessions.lock();
                    sessions
                        .get(&session_id)
                        .is_some_and(|(active_runtime_id, _)| *active_runtime_id == runtime_id)
                        .then(|| sessions.remove(&session_id))
                        .flatten()
                };
                if let Some((_, driver)) = removed.as_ref() {
                    self.persist_live_snapshot(session_id, driver)?;
                    self.seal_session_trajectory(session_id);
                }
                drop(removed);
                Ok(ResponsePayload::Ack)
            }
            Command::QueryTrajectory { query } => self.query_trajectory(session_id, query),
            command => {
                let driver = {
                    let sessions = self.sessions.lock();
                    let (active_runtime_id, driver) = sessions
                        .get(&session_id)
                        .ok_or_else(|| anyhow!("daemon session {session_id} is not running"))?;
                    if *active_runtime_id != runtime_id {
                        bail!(
                            "daemon session {session_id} belongs to runtime {active_runtime_id}, not {runtime_id}"
                        );
                    }
                    driver.clone()
                };
                handle_driver_command(self, session_id, runtime_id, &driver, command)
            }
        }
    }

    fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock());
        for (session_id, (_, driver)) in &sessions {
            self.persist_live_snapshot(*session_id, driver).ok();
            self.seal_session_trajectory(*session_id);
        }
        drop(sessions);
        let terminals = std::mem::take(&mut *self.terminals.lock());
        drop(terminals);
    }

    fn bind_event_hub(&self, hub: EventHub) {
        *self.live_hub.lock() = Some(hub);
    }
}

impl WakuBackend {
    fn prepare_embedded_start(
        &self,
        session_id: Uuid,
        task: Option<StartTask>,
        previous_runtime: Option<Uuid>,
    ) -> anyhow::Result<(wakuwaku_harness::SessionSnapshot, Option<u64>)> {
        let task_generation = self.ensure_start_task(session_id, task, previous_runtime)?;
        Ok((self.embedded_snapshot(session_id)?, task_generation))
    }

    fn ensure_start_task(
        &self,
        session_id: Uuid,
        task: Option<StartTask>,
        previous_runtime: Option<Uuid>,
    ) -> anyhow::Result<Option<u64>> {
        let Some(task) = task else {
            let state = self.task_state.lock();
            if state
                .sessions
                .iter()
                .any(|session| session.id == session_id)
            {
                return Ok(None);
            }
            return Err(anyhow!("the task is unavailable"));
        };
        if task.session.id != session_id {
            bail!(
                "start task {} does not match session {session_id}",
                task.session.id
            );
        }
        if self.removed_session_ids.lock().contains(&session_id) {
            return Err(anyhow!("the task is unavailable"));
        }
        let mut state = self.task_state.lock();
        if let Some(existing) = state
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        {
            self.task_store
                .hydrate(existing)
                .context("could not hydrate start task")?;
        }
        if let Some(project) = task.project {
            upsert_start_project(&mut state, project);
        }
        upsert_start_session(&mut state, task.session, previous_runtime, task.generation);
        let accepted = state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(AgentSession::transcript_baseline_generation);
        state.mark_session_dirty(session_id);
        self.task_store.save(&mut state)?;
        Ok(accepted)
    }

    fn embedded_snapshot(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<wakuwaku_harness::SessionSnapshot> {
        let mut state = self.task_state.lock();
        let index = state
            .sessions
            .iter()
            .position(|session| session.id == session_id)
            .ok_or_else(|| anyhow!("the task is unavailable"))?;
        self.task_store.hydrate(&mut state.sessions[index])?;

        let snapshot = match self.task_store.load_harness_snapshot(session_id)? {
            Some(snapshot) => snapshot,
            None if session_requires_stored_snapshot(&state.sessions[index]) => {
                if !recover_missing_harness_snapshot(&mut state.sessions[index]) {
                    return Err(missing_harness_snapshot());
                }
                state.mark_session_dirty(session_id);
                self.task_store.save(&mut state)?;
                let snapshot = empty_session_snapshot();
                self.task_store
                    .persist_harness_snapshot(session_id, snapshot.clone())?;
                snapshot
            }
            None => {
                let snapshot = empty_session_snapshot();
                self.task_store
                    .persist_harness_snapshot(session_id, snapshot.clone())?;
                snapshot
            }
        };
        drop(state);
        self.ensure_trajectory_initialized(session_id);
        Ok(snapshot)
    }

    #[cfg(test)]
    fn mark_removed_for_test(&self, session_id: Uuid) {
        self.removed_session_ids.lock().insert(session_id);
    }

    fn store_live_snapshot(&self, session_id: Uuid, driver: &DriverHandle) {
        if let Ok(snapshot) = driver.snapshot() {
            self.task_store.set_harness_snapshot(session_id, snapshot);
        }
    }

    fn persist_live_snapshot(&self, session_id: Uuid, driver: &DriverHandle) -> anyhow::Result<()> {
        persist_driver_snapshot(&self.task_store, session_id, driver);
        Ok(())
    }

    fn required_session_snapshot(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<wakuwaku_harness::SessionSnapshot> {
        let driver = self
            .sessions
            .lock()
            .get(&session_id)
            .map(|(_, driver)| driver.clone());
        if let Some(driver) = driver
            && let Ok(snapshot) = driver.snapshot()
        {
            return Ok(snapshot);
        }
        self.task_store
            .load_harness_snapshot(session_id)?
            .ok_or_else(missing_harness_snapshot)
    }

    fn ensure_trajectory_initialized(&self, session_id: Uuid) {
        ensure_trajectory_initialized(
            &self.task_store,
            &self.task_state,
            &self.trajectory,
            session_id,
        );
    }

    fn seal_session_trajectory(&self, session_id: Uuid) {
        ensure_trajectory_initialized(
            &self.task_store,
            &self.task_state,
            &self.trajectory,
            session_id,
        );
        let _ = self.trace_handoff.finish_and_flush(session_id);
    }

    fn query_trajectory(
        &self,
        session_id: Uuid,
        query: wakuwaku_protocol::TrajectoryQuery,
    ) -> anyhow::Result<ResponsePayload> {
        if session_id.is_nil() {
            bail!("QueryTrajectory requires a session id");
        }
        self.ensure_trajectory_initialized(session_id);
        match query {
            wakuwaku_protocol::TrajectoryQuery::Page {
                before,
                limit,
                at_least_revision,
            } => {
                let page = self
                    .trajectory
                    .page(
                        session_id,
                        before.map(|cursor| (cursor.sequence as i64, cursor.record_id)),
                        limit,
                        at_least_revision.map(|revision| revision as i64),
                    )
                    .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Trajectory {
                    response: Box::new(crate::trajectory::to_page_response(&page)),
                })
            }
            wakuwaku_protocol::TrajectoryQuery::Detail {
                record_id,
                section,
                cursor,
                limit,
                at_least_revision,
            } => {
                let context = self
                    .trajectory
                    .detail_context(
                        session_id,
                        record_id,
                        at_least_revision.map(|revision| revision as i64),
                    )
                    .map_err(|error| anyhow!(error))?;
                Ok(ResponsePayload::Trajectory {
                    response: Box::new(crate::trajectory_detail::project_detail(
                        &context, section, cursor, limit,
                    )),
                })
            }
        }
    }
}

fn session_projection_precedes(
    existing: &AgentSession,
    incoming: &AgentSession,
    active_runtime_id: Option<Uuid>,
) -> bool {
    let existing_cursor = existing.runtime_event_cursor;
    let incoming_cursor = incoming.runtime_event_cursor;
    if let Some(active_runtime_id) = active_runtime_id {
        let existing_is_active =
            existing_cursor.is_some_and(|cursor| cursor.runtime_id == active_runtime_id);
        let incoming_is_active =
            incoming_cursor.is_some_and(|cursor| cursor.runtime_id == active_runtime_id);
        if existing_is_active != incoming_is_active {
            return existing_is_active;
        }
    }
    match (existing_cursor, incoming_cursor) {
        (Some(existing), Some(incoming))
            if existing.runtime_id == incoming.runtime_id && existing.epoch == incoming.epoch =>
        {
            incoming.sequence < existing.sequence
        }
        (Some(_), None) if existing.status.is_busy() => true,
        _ => incoming.updated_at < existing.updated_at,
    }
}

fn merge_stale_session_metadata(existing: &mut AgentSession, incoming: AgentSession) {
    if incoming.updated_at >= existing.updated_at {
        existing.title = incoming.title;
        existing.project_id = incoming.project_id;
        existing.workspace = incoming.workspace;
        existing.provider = incoming.provider;
        existing.model = incoming.model;
        existing.runtime_mode = incoming.runtime_mode;
        existing.interaction_mode = incoming.interaction_mode;
        existing.reasoning_effort = incoming.reasoning_effort;
        existing.service_tier = incoming.service_tier;
        existing.context_window = incoming.context_window;
        existing.updated_at = incoming.updated_at;
        existing.last_reply_at = incoming.last_reply_at.or(existing.last_reply_at);
    }
    for queued in incoming.queued_messages {
        if !existing
            .queued_messages
            .iter()
            .any(|candidate| candidate.id == queued.id)
        {
            existing.queued_messages.push(queued);
        }
    }
}

/// Ending checkpoints are produced and stored by the daemon. A second client
/// may still save a projection created just before capture completed; never
/// let that stale projection erase the canonical Git snapshot.
fn preserve_daemon_checkpoints(existing: &AgentSession, incoming: &mut AgentSession) {
    for turn in &mut incoming.turns {
        let Some(checkpoint) = existing
            .turns
            .iter()
            .find(|candidate| candidate.turn_count == turn.turn_count)
            .and_then(|candidate| candidate.checkpoint.as_ref())
            .filter(|checkpoint| {
                matches!(
                    checkpoint.status,
                    CheckpointStatus::Ready | CheckpointStatus::Unavailable
                )
            })
        else {
            continue;
        };
        turn.checkpoint = Some(checkpoint.clone());
    }
}

impl WakuBackend {
    /// Fork a persisted transcript. The embedded harness has no provider-native
    /// cursor; the daemon owns the copied transcript and starts a fresh HTTP
    /// session when the user submits the next prompt.
    fn fork_session_from_response(
        &self,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<(AgentSession, Option<String>)> {
        let (source, cwd, fork_title) = {
            let mut state = self.task_state.lock();
            let index = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
                .ok_or_else(|| anyhow!("the source task is unavailable"))?;
            self.task_store.hydrate(&mut state.sessions[index])?;
            let source = state.sessions[index].clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == source.project_id)
                .ok_or_else(|| anyhow!("the source task project is unavailable"))?;
            let cwd = source.workspace.path().unwrap_or(&project.path).to_owned();
            let fork_title = next_response_fork_title(
                source.display_title(),
                state
                    .sessions
                    .iter()
                    .filter(|session| session.project_id == source.project_id)
                    .map(AgentSession::display_title),
            );
            (source, cwd, fork_title)
        };
        validate_response_fork(&source, turn_count)?;
        let _ = self.trajectory.flush(session_id);
        let source_snapshot = self.required_session_snapshot(session_id)?;
        let forked_snapshot = fork_session_snapshot(&source_snapshot, turn_count)?;
        let mut forked = source
            .fork_through_turn(turn_count, &fork_title)
            .ok_or_else(|| anyhow!("the selected response cannot be copied"))?;
        for turn in &mut forked.turns {
            if let Some(checkpoint) = turn.checkpoint.as_mut() {
                checkpoint.git_ref =
                    crate::checkpoint::checkpoint_ref(forked.id, checkpoint.turn_count);
            }
        }
        let checkpoint_warning =
            crate::checkpoint::copy_session_refs(&cwd, source.id, forked.id, turn_count)
                .err()
                .map(|error| error.to_string());
        self.task_store
            .persist_harness_snapshot(forked.id, forked_snapshot)?;
        let dest_id = forked.id;
        let mut state = self.task_state.lock();
        state.push_session(forked.clone());
        if let Err(error) = self.task_store.save(&mut state) {
            self.task_store.unlink_harness_snapshot(dest_id);
            return Err(error.into());
        }
        if let Err(error) = self.trajectory.fork(session_id, dest_id) {
            let _ = self.trajectory.mark_error(dest_id, error.to_string());
        }
        Ok((forked, checkpoint_warning))
    }

    /// Rewind the daemon-owned transcript and worktree. The next prompt starts
    /// a fresh embedded HTTP conversation from the retained messages.
    fn rewind_session_to_message(
        &self,
        session_id: Uuid,
        turn_count: usize,
    ) -> anyhow::Result<(AgentSession, Option<String>)> {
        let (mut source, cwd) = {
            let mut state = self.task_state.lock();
            let index = state
                .sessions
                .iter()
                .position(|session| session.id == session_id)
                .ok_or_else(|| anyhow!("the task is unavailable"))?;
            self.task_store.hydrate(&mut state.sessions[index])?;
            let source = state.sessions[index].clone();
            let project = state
                .projects
                .iter()
                .find(|project| project.id == source.project_id)
                .ok_or_else(|| anyhow!("the task project is unavailable"))?;
            (
                source.clone(),
                source.workspace.path().unwrap_or(&project.path).to_owned(),
            )
        };
        validate_message_rewind(&source, turn_count)?;
        let retained = turn_count.saturating_sub(1);
        let snapshot =
            rewind_session_snapshot(self.required_session_snapshot(session_id)?, retained)?;
        let restore_ref = crate::checkpoint::turn_start_ref(session_id, turn_count);
        if crate::checkpoint::has_ref(&cwd, &restore_ref) {
            crate::checkpoint::restore_ref(&cwd, &restore_ref)?;
        }
        let removed = self.sessions.lock().remove(&session_id);
        drop(removed);
        let _ = self.trajectory.flush(session_id);
        source.truncate_after_turn(retained);
        source.status = SessionStatus::Idle;
        self.task_store
            .persist_harness_snapshot(session_id, snapshot)?;
        if let Err(error) = self.trajectory.rewind(session_id, retained as i64) {
            let _ = self.trajectory.mark_error(session_id, error.to_string());
        }
        let mut state = self.task_state.lock();
        let existing = state
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
            .ok_or_else(|| anyhow!("the task was removed while it was being rewound"))?;
        *existing = source.clone();
        state.mark_session_dirty(session_id);
        self.task_store.save(&mut state)?;
        Ok((source, None))
    }
}

fn persist_driver_snapshot(store: &StateStore, session_id: Uuid, driver: &DriverHandle) {
    let Ok(snapshot) = driver.snapshot() else {
        return;
    };
    let _ = store.persist_harness_snapshot(session_id, snapshot);
}

fn missing_harness_snapshot() -> anyhow::Error {
    anyhow!("persisted harness snapshot is missing; cannot reconstruct a live transcript")
}

fn empty_session_snapshot() -> wakuwaku_harness::SessionSnapshot {
    wakuwaku_harness::Session::new(Some(crate::driver::WAKU_SYSTEM_PROMPT.to_owned())).snapshot()
}

fn persist_usage_event(
    store: &StateStore,
    task_state: &Mutex<PersistedState>,
    session_id: Uuid,
    event: &DriverEvent,
) -> anyhow::Result<bool> {
    let DriverEvent::UsageUpdated {
        event_id,
        provider,
        model,
        timestamp_ms,
        input,
        output,
        cache_read,
        cache_write,
        reasoning,
        ..
    } = event
    else {
        return Ok(false);
    };
    if *input == 0 && *output == 0 && *cache_read == 0 && *cache_write == 0 {
        return Ok(false);
    }
    let project_path = {
        let state = task_state.lock();
        state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .and_then(|session| {
                state
                    .projects
                    .iter()
                    .find(|project| project.id == session.project_id)
                    .map(|project| project.path.display().to_string())
            })
            .unwrap_or_default()
    };
    store
        .insert_usage_event(&crate::usage_history::UsageEvent {
            event_id: *event_id,
            session_id,
            project_path,
            provider: provider.clone(),
            model: model.clone(),
            timestamp_ms: *timestamp_ms,
            input: *input,
            output: *output,
            cache_read: *cache_read,
            cache_write: *cache_write,
            reasoning: *reasoning,
        })
        .map_err(|error| {
            anyhow!("could not persist usage event {event_id} for session {session_id}: {error}")
        })
}

fn report_usage_persist_error(error: &anyhow::Error) {
    eprintln!("wakuwaku-daemon: {error}");
}

fn report_snapshot_persist_error(error: &std::io::Error) {
    eprintln!("wakuwaku-daemon: could not persist admitted prompt snapshot: {error}");
}

fn report_shadow_enqueue_failure(reason: &str) {
    eprintln!("wakuwaku-daemon: shadow session event enqueue failed: {reason}");
}

/// Snapshot of the daemon's shadow-log wiring handed to each forwarding
/// thread. Maps only events this daemon actually produces; deltas, cursor,
/// activity, and rich activity stay live-only.
#[derive(Clone)]
struct ShadowEventCapture {
    enabled: bool,
    writer: Arc<TrajectoryWriter>,
}

impl ShadowEventCapture {
    fn observe(&self, session_id: Uuid, runtime_id: Uuid, event: &DriverEvent) {
        if !self.enabled {
            return;
        }
        use crate::model::BackgroundWorkEvent;
        use crate::session_events::{SessionEventPayload, bounded_text, sha256_hex};
        let payload = match event {
            DriverEvent::TurnStarted => Some(SessionEventPayload::TurnStarted {}),
            DriverEvent::UsageUpdated {
                event_id,
                provider,
                model,
                timestamp_ms,
                input,
                output,
                cache_read,
                cache_write,
                reasoning,
                context_tokens,
                context_window,
            } => Some(SessionEventPayload::UsageRecorded {
                usage_event_id: *event_id,
                provider: provider.as_str().to_owned(),
                model: model.clone(),
                timestamp_ms: *timestamp_ms,
                input: *input,
                output: *output,
                cache_read: *cache_read,
                cache_write: *cache_write,
                reasoning: *reasoning,
                context_tokens: *context_tokens,
                context_window: *context_window,
            }),
            DriverEvent::TurnFinished { success, summary } => {
                Some(SessionEventPayload::TurnFinished {
                    success: *success,
                    summary: summary.clone().map(bounded_text),
                })
            }
            DriverEvent::Permission {
                request_id, title, ..
            } => Some(SessionEventPayload::PermissionRequested {
                request_id: request_id.clone(),
                title: bounded_text(title.clone()),
            }),
            DriverEvent::UserInputRequested {
                request_id,
                questions,
            } => Some(SessionEventPayload::UserInputRequested {
                request_id: request_id.clone(),
                question_count: questions.len(),
            }),
            DriverEvent::SteerAccepted { message } => Some(SessionEventPayload::SteerAccepted {
                digest: sha256_hex(message),
            }),
            DriverEvent::SteerRejected { message, reason } => {
                Some(SessionEventPayload::SteerRejected {
                    digest: sha256_hex(message),
                    reason: bounded_text(reason.clone()),
                })
            }
            DriverEvent::BackgroundWork(boxed) => match &**boxed {
                BackgroundWorkEvent::Upsert(item) => Some(SessionEventPayload::BackgroundWork {
                    work_kind: item.key.kind,
                    provider_id: item.key.provider_id.clone(),
                    status: item.status,
                }),
                // Output deltas, reconciliation snapshots, and stop events
                // carry command/output/cwd data and stay live-only.
                _ => None,
            },
            DriverEvent::Error(message) => Some(SessionEventPayload::Error {
                message: bounded_text(message.clone()),
            }),
            DriverEvent::ProcessExited => Some(SessionEventPayload::ProcessExited {}),
            DriverEvent::RuntimeEventCursorAdvanced(_)
            | DriverEvent::Connected
            | DriverEvent::AutoTitleUpdated(_)
            | DriverEvent::TextDelta(_)
            | DriverEvent::ReasoningDelta(_)
            | DriverEvent::Activity { .. }
            | DriverEvent::RichActivity(_) => None,
        };
        let Some(payload) = payload else {
            return;
        };
        // Usage rows reuse the legacy usage event id so the shadow stream and
        // `usage_events` share one identity; all others get a fresh id.
        let event = crate::session_events::NewSessionEvent {
            event_id: match event {
                DriverEvent::UsageUpdated { event_id, .. } => *event_id,
                _ => Uuid::new_v4(),
            },
            ..crate::session_events::NewSessionEvent::observed(Some(runtime_id), None, payload)
        };
        match self
            .writer
            .try_append_session_events(session_id, vec![event])
        {
            crate::trajectory_store::SessionEventEnqueue::Queued => {}
            crate::trajectory_store::SessionEventEnqueue::Full => {
                report_shadow_enqueue_failure("writer queue full");
            }
            crate::trajectory_store::SessionEventEnqueue::Disconnected => {
                report_shadow_enqueue_failure("writer disconnected");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_and_forward_driver_event(
    store: &StateStore,
    task_state: &Mutex<PersistedState>,
    handoff: &TraceHandoff,
    session_id: Uuid,
    event: DriverEvent,
    on_turn_finished: impl FnOnce(),
    deliver_events: &mut bool,
    send: impl Fn(WireDriverEvent) -> bool,
) {
    if let Err(error) = persist_usage_event(store, task_state, session_id, &event) {
        report_usage_persist_error(&error);
    }
    let finished = matches!(event, DriverEvent::TurnFinished { .. });
    if finished {
        on_turn_finished();
        let _ = handoff.finish_and_flush(session_id);
    }
    if !*deliver_events {
        return;
    }
    let wire = event_to_wire(event).unwrap_or_else(|error| {
        WireDriverEvent::new(
            "error",
            Value::String(format!("could not encode daemon event: {error}")),
        )
    });
    if !send(wire) {
        *deliver_events = false;
    }
}

fn ensure_trajectory_initialized(
    store: &StateStore,
    task_state: &Mutex<PersistedState>,
    writer: &TrajectoryWriter,
    session_id: Uuid,
) {
    let source = match store.read_snapshot_file_only(session_id) {
        Ok(Some(snapshot)) => TrajectoryInitSource::Snapshot(snapshot),
        Ok(None) => {
            let mut state = task_state.lock();
            match state
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                Some(session) => match store.hydrate(session) {
                    Ok(()) => TrajectoryInitSource::LegacyPartial(Box::new(session.clone())),
                    Err(_) => TrajectoryInitSource::Empty,
                },
                None => TrajectoryInitSource::Empty,
            }
        }
        Err(error) => {
            let _ = writer.mark_error(session_id, error.to_string());
            return;
        }
    };
    let _ = writer.ensure_initialized(session_id, source);
}

fn trajectory_user_from_resolved(
    message: &wakuwaku_harness::UserMessage,
    source: &TrajectoryInputSource,
) -> TrajectoryUserInput {
    let has_image = source.attachments.iter().any(|item| match item {
        TrajectoryAttachmentSource::Recorded { is_image, .. } => *is_image,
        TrajectoryAttachmentSource::ImageMetadataUnavailable { .. } => true,
    });
    let source_metadata_missing = source.attachments.iter().any(|item| {
        matches!(
            item,
            TrajectoryAttachmentSource::ImageMetadataUnavailable { .. }
        )
    });
    TrajectoryUserInput {
        text: wakuwaku_harness::UserMessage::text_of(&message.parts),
        display_text: source.display_text.clone(),
        has_image,
        source_metadata_missing,
        attachment_labels: source
            .attachments
            .iter()
            .map(|item| match item {
                TrajectoryAttachmentSource::Recorded { mention, name, .. } => {
                    if mention.is_empty() {
                        name.clone()
                    } else {
                        mention.clone()
                    }
                }
                TrajectoryAttachmentSource::ImageMetadataUnavailable { .. } => "image".into(),
            })
            .collect(),
    }
}

fn session_requires_stored_snapshot(session: &AgentSession) -> bool {
    session.turns.iter().any(|turn| turn.provider_turn_started)
}

/// Repairs a session whose stored harness snapshot vanished while every
/// provider-started turn is an uncompleted suffix. Those turns could only
/// have been in flight when the snapshot was lost, so their provider context
/// is unrecoverable and the suffix is truncated back to pre-provider
/// history; completed provider history still requires the real snapshot.
fn recover_missing_harness_snapshot(session: &mut AgentSession) -> bool {
    let Some(first_provider_started) = session
        .turns
        .iter()
        .position(|turn| turn.provider_turn_started)
    else {
        return false;
    };
    if session.turns[first_provider_started..]
        .iter()
        .any(|turn| turn.status == TurnStatus::Completed)
    {
        return false;
    }
    session.truncate_after_turn(first_provider_started);
    session.status = SessionStatus::Idle;
    session.runtime_event_cursor = None;
    for message in &mut session.messages {
        message.streaming = false;
    }
    for block in &mut session.transcript_blocks {
        for activity in &mut block.activities {
            activity.complete = true;
        }
    }
    session
        .transcript_blocks
        .retain(|block| !block.activities.is_empty());
    session.updated_at = unix_time();
    !session_requires_stored_snapshot(session)
}

fn upsert_start_project(state: &mut PersistedState, project: Project) {
    if let Some(existing) = state
        .projects
        .iter_mut()
        .find(|existing| existing.id == project.id)
    {
        *existing = project;
    } else {
        state.projects.push(project);
    }
}

fn upsert_start_session(
    state: &mut PersistedState,
    mut session: AgentSession,
    active_runtime: Option<Uuid>,
    generation: u64,
) {
    if let Some(existing) = state
        .sessions
        .iter_mut()
        .find(|existing| existing.id == session.id)
    {
        let stale = generation != existing.transcript_baseline_generation()
            || session_projection_precedes(existing, &session, active_runtime);
        if stale {
            merge_stale_session_metadata(existing, session);
        } else {
            preserve_daemon_checkpoints(existing, &mut session);
            *existing = session;
        }
    } else {
        state.sessions.push(session);
    }
}

fn fork_session_snapshot(
    snapshot: &wakuwaku_harness::SessionSnapshot,
    completed_turns: usize,
) -> anyhow::Result<wakuwaku_harness::SessionSnapshot> {
    wakuwaku_harness::Session::with_snapshot(snapshot.clone())
        .and_then(|session| session.fork_completed_turns(completed_turns))
        .map(|session| session.snapshot())
        .map_err(|error| anyhow!(error.to_string()))
}

fn rewind_session_snapshot(
    snapshot: wakuwaku_harness::SessionSnapshot,
    completed_turns: usize,
) -> anyhow::Result<wakuwaku_harness::SessionSnapshot> {
    let mut session = wakuwaku_harness::Session::with_snapshot(snapshot)
        .map_err(|error| anyhow!(error.to_string()))?;
    session
        .truncate_completed_turns(completed_turns)
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(session.snapshot())
}

fn validate_response_fork(source: &AgentSession, turn_count: usize) -> anyhow::Result<()> {
    if !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed) {
        bail!("stop the task before forking a response");
    }
    if turn_count == 0 || turn_count > source.turns.len() {
        bail!("the selected response is unavailable");
    }
    Ok(())
}

fn validate_message_rewind(source: &AgentSession, turn_count: usize) -> anyhow::Result<()> {
    if !matches!(source.status, SessionStatus::Idle | SessionStatus::Failed) {
        bail!("stop the task before editing a prior message");
    }
    let Some(turn) = source
        .turns
        .iter()
        .find(|turn| turn.turn_count == turn_count)
    else {
        bail!("the selected message is unavailable");
    };
    if !source.messages.iter().any(|message| {
        message.turn_id == Some(turn.id) && message.role == crate::model::MessageRole::User
    }) {
        bail!("the selected user message is unavailable");
    }
    Ok(())
}

fn next_response_fork_title<'a>(base: &str, titles: impl IntoIterator<Item = &'a str>) -> String {
    let existing: HashSet<String> = titles.into_iter().map(str::to_owned).collect();
    let stem = fork_title_stem(base, &existing);
    if !existing.contains(stem) && stem == base {
        return base.to_owned();
    }
    let prefix = format!("{stem} (");
    let mut highest = 1;
    for title in &existing {
        if title == stem {
            continue;
        }
        let Some(suffix) = title
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix(')'))
        else {
            continue;
        };
        if let Ok(number) = suffix.parse::<u32>() {
            highest = highest.max(number);
        }
    }
    format!("{stem} ({})", highest + 1)
}

fn fork_title_stem<'a>(base: &'a str, existing: &HashSet<String>) -> &'a str {
    let Some((stem, suffix)) = base.rsplit_once(" (") else {
        return base;
    };
    if !suffix.ends_with(')') {
        return base;
    }
    let digits = &suffix[..suffix.len() - 1];
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return base;
    }
    if existing.contains(stem) { stem } else { base }
}

/// Safe prompt-source metadata for trajectory recording.
/// Split from [`wakuwaku_protocol::PromptInput`] before filesystem or base64
/// conversion so host paths and image bytes never enter the ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrajectoryInputSource {
    pub display_text: Option<String>,
    pub attachments: Vec<TrajectoryAttachmentSource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrajectoryAttachmentSource {
    Recorded {
        reference: Option<String>,
        mention: String,
        name: String,
        is_dir: bool,
        is_image: bool,
        mime: Option<String>,
    },
    /// Provider image existed only as resolved bytes; mention/name were not recorded.
    ImageMetadataUnavailable { mime: Option<String> },
}

pub struct ResolvedPromptInput {
    pub message: wakuwaku_harness::UserMessage,
    pub source: TrajectoryInputSource,
}

pub fn split_trajectory_input_source(
    input: &wakuwaku_protocol::PromptInput,
) -> TrajectoryInputSource {
    let mut attachments = input
        .sources
        .iter()
        .map(|source| TrajectoryAttachmentSource::Recorded {
            reference: source.safe_reference().map(str::to_owned),
            mention: source.mention.clone(),
            name: source.name.clone(),
            is_dir: source.is_dir,
            is_image: source.is_image,
            mime: source.mime.clone(),
        })
        .collect::<Vec<_>>();
    let recorded_refs = input
        .sources
        .iter()
        .filter_map(wakuwaku_protocol::PromptAttachmentSource::safe_reference)
        .collect::<HashSet<_>>();
    for image in &input.attachments {
        let reference = image.reference();
        if recorded_refs.contains(reference) {
            continue;
        }
        attachments.push(TrajectoryAttachmentSource::ImageMetadataUnavailable {
            mime: wakuwaku_protocol::PromptAttachmentSource::mime_from_name(reference),
        });
    }
    TrajectoryInputSource {
        display_text: input.display_text.clone(),
        attachments,
    }
}

fn resolve_prompt_input(
    backend: &WakuBackend,
    input: wakuwaku_protocol::PromptInput,
) -> anyhow::Result<ResolvedPromptInput> {
    let source = split_trajectory_input_source(&input);
    for attachment in &input.attachments {
        attachment.validate().map_err(anyhow::Error::msg)?;
    }
    let mut parts = vec![wakuwaku_harness::UserPart::Text(input.text)];
    for attachment in input.attachments {
        let reference = attachment.reference().to_owned();
        let path = match &attachment {
            wakuwaku_protocol::PromptImageRef::Blob { reference } => {
                backend.task_store.blobs().path_for(reference)
            }
            wakuwaku_protocol::PromptImageRef::Attachment { reference } => {
                backend.attachments.path_for(reference)
            }
        }
        .ok_or_else(|| anyhow!("unknown image reference {reference}"))?;
        let bytes = std::fs::read(&path)
            .with_context(|| format!("could not read image {}", path.display()))?;
        let mime = match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => bail!("image reference {reference} has no supported MIME type"),
        };
        parts.push(wakuwaku_harness::UserPart::Image {
            mime_type: mime.into(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(ResolvedPromptInput {
        message: wakuwaku_harness::UserMessage { parts },
        source,
    })
}

fn ensure_fresh_driver_auth(
    backend: &WakuBackend,
    session_id: Uuid,
    driver: &DriverHandle,
) -> anyhow::Result<()> {
    backend
        .auth
        .set_custom_providers(backend.settings.get().external_providers);
    let (provider_id, model) = {
        let state = backend.task_state.lock();
        let session = state
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| anyhow!("the task is unavailable"))?;
        (session.provider.clone(), session.model.clone())
    };
    let (_endpoint, _transport, auth, extra, _capabilities, _limits) = backend
        .auth
        .overlay_for_model(&provider_id, model.as_deref())?;
    driver.replace_auth(auth, extra)
}

fn handle_driver_command(
    backend: &WakuBackend,
    session_id: Uuid,
    runtime_id: Uuid,
    driver: &DriverHandle,
    command: Command,
) -> anyhow::Result<ResponsePayload> {
    match command {
        Command::Prompt { input } => {
            ensure_fresh_driver_auth(backend, session_id, driver)?;
            backend.ensure_trajectory_initialized(session_id);
            let resolved = resolve_prompt_input(backend, input)?;
            let digest = crate::session_events::sha256_hex(
                &wakuwaku_harness::UserMessage::text_of(&resolved.message.parts),
            );
            let display_text = resolved
                .source
                .display_text
                .clone()
                .map(crate::session_events::bounded_text);
            let attachment_count = resolved.source.attachments.len();
            let turn_id = {
                let state = backend.task_state.lock();
                state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .and_then(|session| {
                        let running: Vec<_> = session
                            .turns
                            .iter()
                            .filter(|turn| turn.status == TurnStatus::Running)
                            .collect();
                        (running.len() == 1).then(|| running[0].id)
                    })
            };
            backend.trace_handoff.stage_user(
                session_id,
                trajectory_user_from_resolved(&resolved.message, &resolved.source),
            );
            if turn_id.is_some() {
                backend.observe_session_event(
                    session_id,
                    Some(runtime_id),
                    turn_id,
                    crate::session_events::SessionEventPayload::PromptObserved {
                        digest,
                        display_text,
                        attachment_count,
                    },
                );
            }
            driver.prompt(resolved.message)
        }
        Command::Steer { input } => {
            ensure_fresh_driver_auth(backend, session_id, driver)?;
            let resolved = resolve_prompt_input(backend, input)?;
            backend.trace_handoff.stage_steer(
                session_id,
                trajectory_user_from_resolved(&resolved.message, &resolved.source),
            );
            driver.steer(resolved.message)
        }
        Command::Cancel => driver.cancel(),
        Command::Respond {
            request_id,
            option_id,
        } => driver.respond(request_id, option_id),
        Command::RespondUserInput {
            request_id,
            answers,
        } => driver.respond_user_input(request_id, answers)?,
        Command::RefreshBackgroundWork => {}
        Command::StopBackgroundWork { key, control_id } => {
            let _ = control_id;
            driver.reject_background_stop(key);
        }
        Command::ApplyOptions { options } => {
            let provider_id = {
                let state = backend.task_state.lock();
                state
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.provider.clone())
                    .ok_or_else(|| anyhow!("the task is unavailable"))?
            };
            backend
                .auth
                .set_custom_providers(backend.settings.get().external_providers);
            let (provider, transport, auth, extra_auth_headers, capabilities, limits) = backend
                .auth
                .overlay_for_model(&provider_id, options.model.as_deref())?;
            let reasoning_effort = backend.auth.resolve_reasoning_effort(
                &provider_id,
                options.model.as_deref(),
                options.reasoning_effort.as_deref(),
            );
            let service_tier = options.service_tier.filter(|_| capabilities.service_tier);
            let applied_model = options.model.clone();
            let applied_mode = decode_enum(&options.mode)?;
            let applied_interaction = decode_enum(&options.interaction_mode)?;
            let applied_reasoning = reasoning_effort.as_ref().map(|(id, _)| id.clone());
            let applied_context = options.context_window.clone();
            let applied = driver.apply_options(SessionOptions {
                mode: applied_mode,
                interaction_mode: applied_interaction,
                model: options.model,
                reasoning_effort: reasoning_effort.map(|(_, provider_value)| provider_value),
                service_tier,
                context_window: options.context_window,
                reconfigure: Some(crate::driver::SessionReconfigure {
                    provider,
                    auth,
                    transport,
                    extra_auth_headers,
                    capabilities,
                    limits,
                }),
            });
            if applied {
                {
                    let mut state = backend.task_state.lock();
                    if let Some(session) = state
                        .sessions
                        .iter_mut()
                        .find(|session| session.id == session_id)
                    {
                        if let Some(model) = applied_model.clone() {
                            session.model = Some(model);
                        }
                        session.runtime_mode = applied_mode;
                        session.interaction_mode = applied_interaction;
                        session.reasoning_effort = applied_reasoning;
                        session.service_tier = service_tier;
                        session.context_window = applied_context;
                    }
                    if let Some(model) = applied_model {
                        state.last_model = Some(model);
                    }
                    state.mark_session_dirty(session_id);
                    backend.task_store.save(&mut state)?;
                }
                backend
                    .task_store
                    .persist_harness_snapshot(session_id, driver.snapshot()?)?;
            }
            return Ok(ResponsePayload::OptionsApplied { applied });
        }
        Command::AttachSession
        | Command::Start { .. }
        | Command::GetSettings
        | Command::UpdateSettings { .. }
        | Command::LoadUsageHistory { .. }
        | Command::LoadSkills { .. }
        | Command::SetSkillsEnabled { .. }
        | Command::TrashSkills { .. }
        | Command::LoadTaskState
        | Command::SaveTaskState(_)
        | Command::RemoveSession
        | Command::HydrateSession { .. }
        | Command::SearchSessionMessages { .. }
        | Command::LoadComposerDrafts
        | Command::SaveComposerDrafts { .. }
        | Command::ApplyComposerDraftChanges { .. }
        | Command::StoreBlob { .. }
        | Command::ImportAttachment { .. }
        | Command::ImportPathAttachment { .. }
        | Command::ReadBlob { .. }
        | Command::ReadAttachment { .. }
        | Command::SweepBlobs
        | Command::ForkSessionFromResponse { .. }
        | Command::RewindSessionToMessage { .. }
        | Command::Workspace { .. }
        | Command::OpenTerminal { .. }
        | Command::WriteTerminal { .. }
        | Command::ResizeTerminal { .. }
        | Command::CloseTerminal
        | Command::CloseSession
        | Command::GetAuthStatus { .. }
        | Command::StartLogin { .. }
        | Command::CompleteApiKeyLogin { .. }
        | Command::CancelLogin { .. }
        | Command::Logout { .. }
        | Command::ListModels { .. }
        | Command::RefreshModels { .. }
        | Command::QueryTrajectory { .. } => {
            bail!("daemon received a command in the wrong dispatch path")
        }
    }
    Ok(ResponsePayload::Ack)
}

fn ensure_shell_environment() {
    static REFRESHED: OnceLock<()> = OnceLock::new();
    REFRESHED.get_or_init(|| {
        crate::command_env::refresh_from_default_shell();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageRole, ProviderId, TurnStatus};
    use std::sync::Arc;
    use wakuwaku_harness::{
        AssistantMessage, ContentBlock, Message, QueueMode, StopReason, TextBlock, ThinkingBlock,
        ToolCall, ToolResult, ToolResultPart, Usage, UserMessage,
    };

    #[test]
    fn trajectory_source_keeps_safe_metadata_and_strips_host_paths() {
        let input = wakuwaku_protocol::PromptInput {
            text: "look at @notes.md".into(),
            display_text: Some("look at".into()),
            attachments: vec![wakuwaku_protocol::PromptImageRef::Blob {
                reference: "wakuwaku-blob:shot.png".into(),
            }],
            sources: vec![
                wakuwaku_protocol::PromptAttachmentSource {
                    reference: Some("/var/waku/attachments/shot.png".into()),
                    mention: "shot.png".into(),
                    name: "shot.png".into(),
                    is_dir: false,
                    is_image: true,
                    mime: Some("image/png".into()),
                },
                wakuwaku_protocol::PromptAttachmentSource::from_named_attachment(
                    Some("wakuwaku-blob:shot.png".into()),
                    "shot.png",
                    "shot.png",
                    false,
                    true,
                ),
            ],
        };
        let source = split_trajectory_input_source(&input);
        assert_eq!(source.display_text.as_deref(), Some("look at"));
        assert_eq!(
            source.attachments,
            vec![
                TrajectoryAttachmentSource::Recorded {
                    reference: None,
                    mention: "shot.png".into(),
                    name: "shot.png".into(),
                    is_dir: false,
                    is_image: true,
                    mime: Some("image/png".into()),
                },
                TrajectoryAttachmentSource::Recorded {
                    reference: Some("wakuwaku-blob:shot.png".into()),
                    mention: "shot.png".into(),
                    name: "shot.png".into(),
                    is_dir: false,
                    is_image: true,
                    mime: Some("image/png".into()),
                },
            ]
        );
        let rendered = format!("{source:?}");
        assert!(!rendered.contains("/var/waku"));
        assert!(!rendered.contains("base64"));
    }

    #[test]
    fn image_refs_without_sources_are_marked_metadata_unavailable() {
        let input = wakuwaku_protocol::PromptInput {
            text: "inspect".into(),
            display_text: None,
            attachments: vec![wakuwaku_protocol::PromptImageRef::Blob {
                reference: "wakuwaku-blob:only.png".into(),
            }],
            sources: Vec::new(),
        };
        let source = split_trajectory_input_source(&input);
        assert_eq!(
            source.attachments,
            vec![TrajectoryAttachmentSource::ImageMetadataUnavailable {
                mime: Some("image/png".into()),
            }]
        );
    }

    #[test]
    fn data_url_references_never_enter_trajectory_source() {
        let input = wakuwaku_protocol::PromptInput {
            text: "photo".into(),
            display_text: None,
            attachments: Vec::new(),
            sources: vec![wakuwaku_protocol::PromptAttachmentSource {
                reference: Some("data:image/png;base64,aaaa".into()),
                mention: "photo.png".into(),
                name: "photo.png".into(),
                is_dir: false,
                is_image: true,
                mime: Some("image/png".into()),
            }],
        };
        let source = split_trajectory_input_source(&input);
        assert_eq!(
            source.attachments,
            vec![TrajectoryAttachmentSource::Recorded {
                reference: None,
                mention: "photo.png".into(),
                name: "photo.png".into(),
                is_dir: false,
                is_image: true,
                mime: Some("image/png".into()),
            }]
        );
    }

    #[test]
    fn stale_runtime_projection_keeps_newer_transcript_cursor() {
        let runtime_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let mut existing = AgentSession::new(
            Uuid::new_v4(),
            ProviderId::new(ProviderId::OPENAI_RESPONSES),
        );
        existing.status = SessionStatus::Working;
        existing.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 10,
        });
        existing.push_message(crate::model::MessageRole::Assistant, "complete so far");

        let mut stale = existing.clone();
        stale.title = "Renamed elsewhere".into();
        stale.messages.clear();
        stale.runtime_event_cursor = Some(crate::model::RuntimeEventCursor {
            runtime_id,
            epoch,
            sequence: 7,
        });

        assert!(session_projection_precedes(
            &existing,
            &stale,
            Some(runtime_id)
        ));
        merge_stale_session_metadata(&mut existing, stale);
        assert_eq!(existing.title, "Renamed elsewhere");
        assert_eq!(existing.messages.len(), 1);
        assert_eq!(existing.runtime_event_cursor.unwrap().sequence, 10);
    }

    #[test]
    fn client_projection_cannot_replace_a_daemon_checkpoint() {
        let mut existing = AgentSession::new(
            Uuid::new_v4(),
            ProviderId::new(ProviderId::OPENAI_RESPONSES),
        );
        existing.begin_turn("change it");
        existing.finish_active_turn(crate::model::TurnStatus::Completed);
        let checkpoint = Checkpoint {
            turn_count: 1,
            git_ref: "refs/wakuwaku/canonical".into(),
            status: CheckpointStatus::Ready,
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            created_at: 1,
        };
        existing.turns[0].checkpoint = Some(checkpoint.clone());

        let mut incoming = existing.clone();
        incoming.turns[0].checkpoint = Some(Checkpoint {
            git_ref: "refs/wakuwaku/stale-client".into(),
            ..checkpoint.clone()
        });
        preserve_daemon_checkpoints(&existing, &mut incoming);

        assert_eq!(incoming.turns[0].checkpoint.as_ref(), Some(&checkpoint));
    }

    #[test]
    fn response_fork_titles_follow_one_numbered_sequence() {
        assert_eq!(
            next_response_fork_title("Fix the bug", ["Fix the bug"]),
            "Fix the bug (2)"
        );
        assert_eq!(
            next_response_fork_title(
                "Fix the bug (2)",
                ["Fix the bug", "Fix the bug (2)", "Fix the bug (4)"]
            ),
            "Fix the bug (5)"
        );
        assert_eq!(
            next_response_fork_title("Plan (2026)", ["Plan (2026)"]),
            "Plan (2026) (2)"
        );
    }

    #[test]
    fn message_rewind_requires_a_settled_user_turn() {
        let mut session = AgentSession::new(
            Uuid::new_v4(),
            ProviderId::new(ProviderId::OPENAI_RESPONSES),
        );
        session.begin_turn("change it");
        session.mark_active_turn_provider_started();
        session.finish_active_turn(crate::model::TurnStatus::Completed);

        assert!(validate_message_rewind(&session, 1).is_ok());

        let mut busy = session.clone();
        busy.status = SessionStatus::Working;
        assert!(validate_message_rewind(&busy, 1).is_err());

        let mut missing_message = session;
        missing_message.messages.clear();
        assert!(validate_message_rewind(&missing_message, 1).is_err());
    }

    #[test]
    fn wire_event_round_trip_preserves_ordered_delta_payload() {
        let wire =
            wakuwaku_protocol::event_to_wire(DriverEvent::TextDelta("hello".into())).unwrap();
        assert_eq!(wire.kind, "textDelta");
        assert!(matches!(
            wakuwaku_protocol::event_from_wire(wire).unwrap(),
            DriverEvent::TextDelta(text) if text == "hello"
        ));
    }

    fn identity_snapshot() -> wakuwaku_harness::SessionSnapshot {
        let first = AssistantMessage {
            content: vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "plan".into(),
                    signature: Some("sig-think".into()),
                    redacted: false,
                }),
                ContentBlock::Text(TextBlock {
                    text: "calling".into(),
                    signature: Some("sig-text".into()),
                }),
                ContentBlock::ToolCall(Arc::new(ToolCall {
                    id: "call-1|item-9".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"src/lib.rs"}),
                    thought_signature: Some("sig-tool".into()),
                })),
            ],
            model: "claude-opus".into(),
            provider: "anthropic".into(),
            response_id: Some("resp-abc".into()),
            usage: Usage {
                input: 11,
                output: 7,
                cache_read: 3,
                cache_write: 1,
                reasoning: Some(2),
                total_tokens: 22,
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
        };
        let second = AssistantMessage {
            content: vec![ContentBlock::text("done")],
            model: "claude-opus".into(),
            provider: "anthropic".into(),
            response_id: Some("resp-def".into()),
            usage: Usage {
                input: 20,
                output: 4,
                cache_read: 0,
                cache_write: 0,
                reasoning: None,
                total_tokens: 24,
            },
            stop_reason: StopReason::Stop,
            error_message: None,
        };
        let messages = vec![
            Message::User(UserMessage::text("inspect")),
            Message::Assistant(Arc::new(first)),
            Message::ToolResult(Arc::new(ToolResult {
                tool_call_id: "call-1|item-9".into(),
                tool_name: "read".into(),
                content: vec![ToolResultPart::Text("contents".into())],
                is_error: false,
                details: None,
            })),
            Message::User(UserMessage::text("continue")),
            Message::Assistant(Arc::new(second)),
        ];
        wakuwaku_harness::Session::with_history(
            Some("system".into()),
            messages,
            vec![3, 5],
            QueueMode::OneAtATime,
            wakuwaku_harness::Budget::default(),
        )
        .unwrap()
        .snapshot()
    }

    fn assert_first_turn_identity(snapshot: &wakuwaku_harness::SessionSnapshot) {
        let assistant = snapshot
            .messages
            .iter()
            .find_map(Message::as_assistant)
            .expect("first assistant");
        assert_eq!(assistant.response_id.as_deref(), Some("resp-abc"));
        assert_eq!(assistant.usage.input, 11);
        assert_eq!(assistant.usage.output, 7);
        assert_eq!(assistant.usage.total_tokens, 22);
        match &assistant.content[..] {
            [
                ContentBlock::Thinking(thinking),
                ContentBlock::Text(text),
                ContentBlock::ToolCall(call),
            ] => {
                assert_eq!(thinking.signature.as_deref(), Some("sig-think"));
                assert_eq!(text.signature.as_deref(), Some("sig-text"));
                assert_eq!(call.id, "call-1|item-9");
                assert_eq!(call.thought_signature.as_deref(), Some("sig-tool"));
            }
            other => panic!("unexpected first-turn content: {other:?}"),
        }
        let result = snapshot.messages.iter().find_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        });
        assert_eq!(
            result.map(|result| result.tool_call_id.as_str()),
            Some("call-1|item-9")
        );
    }

    fn two_turn_session(project_id: Uuid) -> AgentSession {
        let mut session =
            AgentSession::new(project_id, ProviderId::new(ProviderId::OPENAI_RESPONSES));
        session.begin_turn("inspect");
        session.mark_active_turn_provider_started();
        session.push_message(MessageRole::Assistant, "calling");
        session.finish_active_turn(TurnStatus::Completed);
        session.begin_turn("continue");
        session.mark_active_turn_provider_started();
        session.push_message(MessageRole::Assistant, "done");
        session.finish_active_turn(TurnStatus::Completed);
        session.status = SessionStatus::Idle;
        session
    }

    fn persist_identity_session(
        directory: &std::path::Path,
    ) -> (Uuid, wakuwaku_harness::SessionSnapshot) {
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let project_id = state.projects[0].id;
        let session = two_turn_session(project_id);
        let session_id = session.id;
        let snapshot = identity_snapshot();
        state.sessions.clear();
        state.push_session(session);
        store
            .persist_harness_snapshot(session_id, snapshot.clone())
            .unwrap();
        store.save(&mut state).unwrap();
        (session_id, snapshot)
    }

    fn backend_in(directory: &std::path::Path) -> WakuBackend {
        WakuBackend::new(
            DaemonSettingsStore::open(directory.join("settings.json")).unwrap(),
            StateStore::daemon(directory.join("app.db")),
        )
        .unwrap()
    }

    #[test]
    fn restart_and_attach_preserve_harness_identity_fields() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-daemon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let (session_id, original) = persist_identity_session(&directory);

        let backend = backend_in(&directory);
        let restored = backend.embedded_snapshot(session_id).unwrap();
        assert_first_turn_identity(&restored);
        assert_eq!(restored.checkpoints.len(), 2);
        assert_eq!(restored.messages.len(), original.messages.len());
        let second = restored
            .messages
            .iter()
            .rev()
            .find_map(Message::as_assistant)
            .unwrap();
        assert_eq!(second.response_id.as_deref(), Some("resp-def"));
        assert_eq!(second.usage.input, 20);

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn query_trajectory_pages_without_a_runtime() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-traj-query-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let (session_id, _) = persist_identity_session(&directory);
        let backend = backend_in(&directory);
        let payload = backend
            .query_trajectory(
                session_id,
                wakuwaku_protocol::TrajectoryQuery::Page {
                    before: None,
                    limit: Some(201),
                    at_least_revision: Some(0),
                },
            )
            .unwrap();
        let ResponsePayload::Trajectory { response } = payload else {
            panic!("expected a trajectory response");
        };
        let wakuwaku_protocol::TrajectoryResponse::Page { rows, revision, .. } = *response else {
            panic!("expected a page");
        };
        assert!(revision >= 1);
        assert!(
            rows.iter()
                .any(|row| row.kind == wakuwaku_protocol::TrajectoryKind::User)
        );
        assert!(
            rows.iter()
                .all(|row| row.search_text.chars().count() <= 2048 + 1 + 512)
        );
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn fork_clones_snapshot_identity_and_leaves_source_unchanged() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-daemon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let (session_id, _) = persist_identity_session(&directory);
        let backend = backend_in(&directory);

        let (forked, _) = backend.fork_session_from_response(session_id, 1).unwrap();
        let source = backend.task_store.harness_snapshot(session_id).unwrap();
        let child = backend.task_store.harness_snapshot(forked.id).unwrap();

        assert_first_turn_identity(&source);
        assert_first_turn_identity(&child);
        assert_eq!(source.checkpoints.len(), 2);
        assert_eq!(child.checkpoints.len(), 1);
        assert_eq!(child.messages.len(), 3);
        assert!(
            source
                .messages
                .iter()
                .rev()
                .find_map(Message::as_assistant)
                .and_then(|assistant| assistant.response_id.as_deref())
                == Some("resp-def")
        );
        assert!(
            child
                .messages
                .iter()
                .rev()
                .find_map(Message::as_assistant)
                .and_then(|assistant| assistant.response_id.as_deref())
                == Some("resp-abc")
        );

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn rewind_truncates_stored_snapshot_without_inventing_identity() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-daemon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let (session_id, _) = persist_identity_session(&directory);
        let backend = backend_in(&directory);

        let (rewound, _) = backend.rewind_session_to_message(session_id, 2).unwrap();
        let snapshot = backend.task_store.harness_snapshot(session_id).unwrap();
        assert_eq!(rewound.turns.len(), 1);
        assert_eq!(snapshot.checkpoints.len(), 1);
        assert_eq!(snapshot.messages.len(), 3);
        assert_first_turn_identity(&snapshot);
        assert!(
            snapshot
                .messages
                .iter()
                .rev()
                .find_map(Message::as_assistant)
                .and_then(|assistant| assistant.response_id.as_deref())
                != Some("resp-def")
        );

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn completed_history_without_harness_snapshot_fails_explicitly() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-daemon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let project_id = state.projects[0].id;
        let session = two_turn_session(project_id);
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let error = backend.embedded_snapshot(session_id).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("persisted harness snapshot is missing")
        );
        assert!(backend.fork_session_from_response(session_id, 1).is_err());

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn new_session_start_uses_empty_snapshot_instead_of_inventing_fields() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-daemon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        state.sessions[0].begin_turn("first prompt");
        let session_id = state.sessions[0].id;
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let snapshot = backend.embedded_snapshot(session_id).unwrap();
        assert!(snapshot.messages.is_empty());
        assert!(snapshot.checkpoints.is_empty());
        assert_eq!(
            snapshot.system_prompt.as_deref(),
            Some(crate::driver::WAKU_SYSTEM_PROMPT)
        );

        std::fs::remove_dir_all(directory).ok();
    }

    fn failed_pre_provider_session(project_id: Uuid) -> AgentSession {
        let mut session =
            AgentSession::new(project_id, ProviderId::new(ProviderId::OPENAI_RESPONSES));
        session.begin_turn("hi");
        session.push_message(
            MessageRole::Assistant,
            "无法启动智能体：the task is unavailable",
        );
        session.finish_active_turn(TurnStatus::Failed);
        session.status = SessionStatus::Failed;
        session
    }

    #[test]
    fn pre_provider_failure_without_snapshot_starts_fresh() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-pre-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session = failed_pre_provider_session(state.projects[0].id);
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let (snapshot, generation) = backend
            .prepare_embedded_start(session_id, None, None)
            .unwrap();
        assert!(generation.is_none());
        assert!(snapshot.messages.is_empty());
        assert!(backend.task_store.harness_snapshot(session_id).is_some());

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn first_turn_persists_empty_snapshot_before_start_failure() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-first-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        state.sessions[0].begin_turn("first prompt");
        let session_id = state.sessions[0].id;
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let _ = backend
            .prepare_embedded_start(session_id, None, None)
            .unwrap();
        assert!(backend.task_store.harness_snapshot(session_id).is_some());
        let error = backend
            .auth
            .overlay_for_model(&ProviderId::new("missing-provider"), Some("no-model"))
            .unwrap_err();
        assert!(error.to_string().contains("not configured"), "{error}");
        assert!(backend.task_store.harness_snapshot(session_id).is_some());

        std::fs::remove_dir_all(directory).ok();
    }

    fn provider_started_failed_session(project_id: Uuid) -> AgentSession {
        let mut session =
            AgentSession::new(project_id, ProviderId::new(ProviderId::OPENAI_RESPONSES));
        session.begin_turn("hi");
        session.mark_active_turn_provider_started();
        session.push_message(MessageRole::Assistant, "partial reply");
        session.finish_active_turn(TurnStatus::Failed);
        session.status = SessionStatus::Failed;
        session
    }

    #[test]
    fn failed_first_provider_turn_without_snapshot_resets_to_pre_provider_history() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-reset-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session = provider_started_failed_session(state.projects[0].id);
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let snapshot = backend.embedded_snapshot(session_id).unwrap();
        assert!(snapshot.messages.is_empty());
        assert!(
            backend
                .task_store
                .read_snapshot_file_only(session_id)
                .unwrap()
                .is_some()
        );
        {
            let state = backend.task_state.lock();
            let session = state
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .unwrap();
            assert!(session.turns.is_empty());
            assert!(session.messages.is_empty());
            assert_eq!(session.status, SessionStatus::Idle);
            assert!(session.runtime_event_cursor.is_none());
        }

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn failed_provider_suffix_after_pre_provider_history_is_truncated() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-suffix-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let mut session = AgentSession::new(
            state.projects[0].id,
            ProviderId::new(ProviderId::OPENAI_RESPONSES),
        );
        session.begin_turn("pre");
        session.finish_active_turn(TurnStatus::Failed);
        session.begin_turn("in-flight");
        session.mark_active_turn_provider_started();
        session.finish_active_turn(TurnStatus::Failed);
        session.status = SessionStatus::Failed;
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let snapshot = backend.embedded_snapshot(session_id).unwrap();
        assert!(snapshot.messages.is_empty());
        {
            let state = backend.task_state.lock();
            let session = state
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .unwrap();
            assert_eq!(session.turns.len(), 1);
            assert!(!session.turns[0].provider_turn_started);
            assert_eq!(session.status, SessionStatus::Idle);
        }

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn failed_provider_suffix_after_completed_provider_history_still_fails_closed() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-closed2-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let mut session = two_turn_session(state.projects[0].id);
        session.turns[1].status = TurnStatus::Failed;
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let error = backend.embedded_snapshot(session_id).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("persisted harness snapshot is missing"),
            "{error}"
        );
        assert!(
            backend
                .task_store
                .read_snapshot_file_only(session_id)
                .unwrap()
                .is_none()
        );

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn provider_started_history_without_snapshot_still_fails_closed() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-closed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let mut session = two_turn_session(state.projects[0].id);
        session.turns[0].status = TurnStatus::Failed;
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let error = backend
            .prepare_embedded_start(session_id, None, None)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("persisted harness snapshot is missing"),
            "{error}"
        );

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn app_local_session_restores_onto_fresh_daemon_db() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-restore-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let backend = backend_in(&directory);
        let project = Project::from_path(directory.join("ws"));
        let session = failed_pre_provider_session(project.id);
        let session_id = session.id;
        let generation = session.transcript_baseline_generation();
        let (_, accepted) = backend
            .prepare_embedded_start(
                session_id,
                Some(StartTask {
                    session,
                    project: Some(project),
                    generation,
                }),
                None,
            )
            .unwrap();
        assert_eq!(accepted, Some(generation));
        assert!(backend.task_store.harness_snapshot(session_id).is_some());

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn unknown_session_without_payload_stays_unavailable() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-miss-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let backend = backend_in(&directory);
        let error = backend
            .prepare_embedded_start(Uuid::new_v4(), None, None)
            .unwrap_err();
        assert!(
            error.to_string().contains("the task is unavailable"),
            "{error}"
        );
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn removed_session_cannot_be_restored_by_start_payload() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-del-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session = failed_pre_provider_session(state.projects[0].id);
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session.clone());
        store.save(&mut state).unwrap();
        let backend = backend_in(&directory);
        backend.mark_removed_for_test(session_id);
        let error = backend
            .prepare_embedded_start(
                session_id,
                Some(StartTask {
                    generation: session.transcript_baseline_generation(),
                    session,
                    project: None,
                }),
                None,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("the task is unavailable"),
            "{error}"
        );
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn stale_start_generation_keeps_newer_daemon_transcript() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-gen-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let mut existing = failed_pre_provider_session(state.projects[0].id);
        existing.title = "daemon newer".into();
        existing.updated_at = 50;
        let session_id = existing.id;
        state.sessions.clear();
        state.push_session(existing.clone());
        store.save(&mut state).unwrap();

        let mut stale = existing;
        stale.title = "client stale".into();
        stale.updated_at = 10;
        let backend = backend_in(&directory);
        backend
            .prepare_embedded_start(
                session_id,
                Some(StartTask {
                    session: stale,
                    project: None,
                    generation: 10,
                }),
                None,
            )
            .unwrap();
        let title = backend
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .unwrap()
            .title
            .clone();
        assert_eq!(title, "daemon newer");
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn equal_timestamp_missing_provider_history_is_rejected() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-daemon-eq-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let mut existing = two_turn_session(state.projects[0].id);
        existing.updated_at = 50;
        let session_id = existing.id;
        let daemon_generation = existing.transcript_baseline_generation();
        state.sessions.clear();
        state.push_session(existing.clone());
        store
            .persist_harness_snapshot(session_id, empty_session_snapshot())
            .unwrap();
        store.save(&mut state).unwrap();

        let mut stale = existing;
        stale.turns.pop();
        stale.messages.truncate(2);
        stale.updated_at = 50;
        let submitted = stale.transcript_baseline_generation();
        assert_ne!(submitted, daemon_generation);

        let backend = backend_in(&directory);
        let (_, accepted) = backend
            .prepare_embedded_start(
                session_id,
                Some(StartTask {
                    session: stale,
                    project: None,
                    generation: submitted,
                }),
                None,
            )
            .unwrap();
        assert_eq!(accepted, Some(daemon_generation));
        assert_ne!(accepted, Some(submitted));
        let kept = backend
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
            .unwrap();
        assert_eq!(kept.turns.len(), 2);
        assert!(kept.turns.iter().all(|turn| turn.provider_turn_started));
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn matching_baseline_accepts_new_unstarted_user_turn() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-unstarted-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let mut existing = two_turn_session(state.projects[0].id);
        existing.updated_at = 50;
        let session_id = existing.id;
        let baseline = existing.transcript_baseline_generation();
        state.sessions.clear();
        state.push_session(existing.clone());
        store
            .persist_harness_snapshot(session_id, empty_session_snapshot())
            .unwrap();
        store.save(&mut state).unwrap();

        let mut incoming = existing;
        incoming.begin_turn("follow up");
        incoming.updated_at = 50;
        assert_eq!(incoming.transcript_baseline_generation(), baseline);
        assert_eq!(incoming.turns.len(), 3);
        assert!(!incoming.turns[2].provider_turn_started);

        let backend = backend_in(&directory);
        let (_, accepted) = backend
            .prepare_embedded_start(
                session_id,
                Some(StartTask {
                    generation: incoming.transcript_baseline_generation(),
                    session: incoming,
                    project: None,
                }),
                None,
            )
            .unwrap();
        assert_eq!(accepted, Some(baseline));
        let stored = backend
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned()
            .unwrap();
        assert_eq!(stored.turns.len(), 3);
        assert!(!stored.turns[2].provider_turn_started);
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn provider_reset_without_provider_history_can_restore() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-daemon-reset-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let mut session = failed_pre_provider_session(state.projects[0].id);
        session.provider = ProviderId::new("opencode-go");
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session.clone());
        store.save(&mut state).unwrap();

        session.provider = ProviderId::new("openai-responses");
        session.updated_at += 1;
        let backend = backend_in(&directory);
        backend
            .prepare_embedded_start(
                session_id,
                Some(StartTask {
                    generation: session.transcript_baseline_generation(),
                    session,
                    project: None,
                }),
                None,
            )
            .unwrap();
        let provider = backend
            .task_state
            .lock()
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .unwrap()
            .provider
            .clone();
        assert_eq!(provider.as_str(), "openai-responses");
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn fresh_system_prompt_survives_fork_and_rewind() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-daemon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session = two_turn_session(state.projects[0].id);
        let session_id = session.id;
        let snapshot = wakuwaku_harness::Session::with_history(
            Some(crate::driver::WAKU_SYSTEM_PROMPT.to_owned()),
            identity_snapshot().messages,
            vec![3, 5],
            wakuwaku_harness::QueueMode::OneAtATime,
            wakuwaku_harness::Budget::default(),
        )
        .unwrap()
        .snapshot();
        state.sessions.clear();
        state.push_session(session);
        store
            .persist_harness_snapshot(session_id, snapshot)
            .unwrap();
        store.save(&mut state).unwrap();

        let backend = backend_in(&directory);
        let (forked, _) = backend.fork_session_from_response(session_id, 1).unwrap();
        let source = backend.task_store.harness_snapshot(session_id).unwrap();
        let child = backend.task_store.harness_snapshot(forked.id).unwrap();
        assert_eq!(
            source.system_prompt.as_deref(),
            Some(crate::driver::WAKU_SYSTEM_PROMPT)
        );
        assert_eq!(source.system_prompt, child.system_prompt);
        backend.rewind_session_to_message(session_id, 2).unwrap();
        let rewound = backend.task_store.harness_snapshot(session_id).unwrap();
        assert_eq!(rewound.system_prompt, source.system_prompt);

        std::fs::remove_dir_all(directory).ok();
    }

    fn billed_usage(event_id: Uuid) -> DriverEvent {
        DriverEvent::UsageUpdated {
            event_id,
            provider: ProviderId::new("anthropic"),
            model: "claude-fable-5".into(),
            timestamp_ms: chrono::Local::now().timestamp_millis(),
            input: 8,
            output: 3,
            cache_read: 1,
            cache_write: 0,
            reasoning: None,
            context_tokens: Some(12),
            context_window: Some(200_000),
        }
    }

    #[test]
    fn billed_usage_event_inserts_once_and_is_not_copied_by_fork() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-usage-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session = two_turn_session(state.projects[0].id);
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store
            .persist_harness_snapshot(session_id, identity_snapshot())
            .unwrap();
        store.save(&mut state).unwrap();
        let event_id = Uuid::from_u128(42);
        let event = billed_usage(event_id);
        let task_state = Mutex::new(state);
        assert!(persist_usage_event(&store, &task_state, session_id, &event).unwrap());
        assert!(!persist_usage_event(&store, &task_state, session_id, &event).unwrap());
        let backend = backend_in(&directory);
        let _ = backend.fork_session_from_response(session_id, 1).unwrap();
        let rows = store.usage_events_between(0, i64::MAX).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id, event_id);
        assert_eq!(rows[0].session_id, session_id);
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn billed_usage_event_is_not_rewritten_by_rewind() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-usage-rw-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session = two_turn_session(state.projects[0].id);
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store
            .persist_harness_snapshot(session_id, identity_snapshot())
            .unwrap();
        store.save(&mut state).unwrap();
        let event_id = Uuid::from_u128(43);
        let event = billed_usage(event_id);
        let task_state = Mutex::new(state);
        assert!(persist_usage_event(&store, &task_state, session_id, &event).unwrap());
        let backend = backend_in(&directory);
        let _ = backend.rewind_session_to_message(session_id, 1).unwrap();
        let rows = store.usage_events_between(0, i64::MAX).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id, event_id);
        assert_eq!(rows[0].session_id, session_id);
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn disconnected_event_sink_still_persists_usage_and_finished_snapshot() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-usage-disc-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session = two_turn_session(state.projects[0].id);
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store.save(&mut state).unwrap();
        let handoff = TraceHandoff::new();
        let task_state = Mutex::new(state);
        let event_id = Uuid::from_u128(77);
        let mut deliver = true;
        let sent = std::sync::atomic::AtomicUsize::new(0);
        let mut finished = false;
        persist_and_forward_driver_event(
            &store,
            &task_state,
            &handoff,
            session_id,
            billed_usage(event_id),
            || unreachable!("usage is not a finished turn"),
            &mut deliver,
            |_| {
                sent.fetch_add(1, std::sync::atomic::Ordering::Release);
                false
            },
        );
        persist_and_forward_driver_event(
            &store,
            &task_state,
            &handoff,
            session_id,
            DriverEvent::TurnFinished {
                success: true,
                summary: Some("done".into()),
            },
            || {
                finished = true;
                store.set_harness_snapshot(session_id, identity_snapshot());
            },
            &mut deliver,
            |_| panic!("disconnected sink must not receive later events"),
        );
        assert!(!deliver);
        assert_eq!(sent.load(std::sync::atomic::Ordering::Acquire), 1);
        assert!(finished);
        let rows = store.usage_events_between(0, i64::MAX).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id, event_id);
        assert!(store.harness_snapshot(session_id).is_some());
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn turn_finished_writes_snapshot_before_trajectory_flush() {
        let directory =
            std::env::temp_dir().join(format!("wakuwaku-traj-order-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session = two_turn_session(state.projects[0].id);
        let session_id = session.id;
        state.sessions.clear();
        state.push_session(session);
        store.save(&mut state).unwrap();
        store
            .persist_harness_snapshot(session_id, identity_snapshot())
            .unwrap();
        let order = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let writer = TrajectoryWriter::open_with_live(store.path(), {
            let order = std::sync::Arc::clone(&order);
            let store_path = store.path().to_path_buf();
            move |_| {
                let exists = store_path
                    .parent()
                    .unwrap()
                    .join("snapshots")
                    .join(format!("{session_id}.json"))
                    .is_file();
                order.lock().push(format!("live:{exists}"));
            }
        })
        .unwrap();
        writer
            .ensure_initialized(session_id, TrajectoryInitSource::Empty)
            .unwrap();
        let writer = std::sync::Arc::new(writer);
        let handoff = TraceHandoff::spawn(std::sync::Arc::clone(&writer)
            as std::sync::Arc<dyn crate::trajectory::TrajectorySubmit>);
        handoff.stage_user(
            session_id,
            TrajectoryUserInput {
                text: "hello".into(),
                ..TrajectoryUserInput::default()
            },
        );
        let mut deliver = true;
        let forwarded = std::sync::atomic::AtomicBool::new(false);
        persist_and_forward_driver_event(
            &store,
            &Mutex::new(state),
            &handoff,
            session_id,
            DriverEvent::TurnFinished {
                success: true,
                summary: Some("done".into()),
            },
            || {
                order.lock().push("snapshot".into());
                store
                    .persist_harness_snapshot(session_id, identity_snapshot())
                    .unwrap();
            },
            &mut deliver,
            |_| {
                order.lock().push("forward".into());
                forwarded.store(true, std::sync::atomic::Ordering::Release);
                true
            },
        );
        let recorded = order.lock().clone();
        let snapshot_at = recorded.iter().position(|step| step == "snapshot");
        let forward_at = recorded.iter().position(|step| step == "forward");
        assert!(snapshot_at.is_some(), "{recorded:?}");
        assert!(forward_at.is_some(), "{recorded:?}");
        assert!(snapshot_at < forward_at, "{recorded:?}");
        assert!(
            recorded.iter().any(|step| step == "live:true"),
            "live update must see the snapshot file: {recorded:?}"
        );
        assert!(forwarded.load(std::sync::atomic::Ordering::Acquire));
        let page = writer.page(session_id, None, Some(50), None).unwrap();
        assert!(
            page.records
                .iter()
                .any(|record| record.kind == crate::trajectory::TrajectoryKind::User)
        );
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn trajectory_fork_rewind_remove_keep_ledger_consistent() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-traj-life-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let (session_id, _) = persist_identity_session(&directory);
        let backend = backend_in(&directory);
        backend.ensure_trajectory_initialized(session_id);
        let _ = backend
            .trajectory
            .apply(crate::trajectory::record_drive(
                session_id,
                Some(TrajectoryUserInput {
                    text: "one".into(),
                    ..TrajectoryUserInput::default()
                }),
                Vec::new(),
                vec![wakuwaku_harness::TraceEvent::PromptPrepared {
                    system_prompt: Some("sys".into()),
                    tools_json: std::sync::Arc::from("[]"),
                    options_json: std::sync::Arc::from("{}"),
                    model_hint: "m".into(),
                }],
            ))
            .unwrap();
        let (forked, _) = backend.fork_session_from_response(session_id, 1).unwrap();
        let forked_page = backend
            .trajectory
            .page(forked.id, None, Some(50), None)
            .unwrap();
        assert!(
            forked_page
                .records
                .iter()
                .any(|record| record.title == "Forked")
        );
        let before = backend
            .trajectory
            .page(session_id, None, Some(50), None)
            .unwrap();
        let _ = backend.rewind_session_to_message(session_id, 1).unwrap();
        let after = backend
            .trajectory
            .page(session_id, None, Some(50), None)
            .unwrap();
        assert_eq!(after.generation, before.generation + 1);
        {
            let mut state = backend.task_state.lock();
            state.sessions.retain(|session| session.id != session_id);
            backend.task_store.save(&mut state).unwrap();
        }
        let removed = backend
            .trajectory
            .page(session_id, None, Some(50), None)
            .unwrap();
        assert!(removed.records.is_empty());
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn persist_usage_event_names_session_and_event_on_db_failure() {
        let directory = std::env::temp_dir().join(format!("wakuwaku-usage-err-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = StateStore::daemon(directory.join("app.db"));
        let mut state = PersistedState::fresh(directory.to_path_buf());
        let session_id = state.sessions[0].id;
        store.save(&mut state).unwrap();
        rusqlite::Connection::open(store.path())
            .unwrap()
            .execute_batch("DROP TABLE usage_events")
            .unwrap();
        let event_id = Uuid::from_u128(99);
        let error = persist_usage_event(
            &store,
            &Mutex::new(state),
            session_id,
            &billed_usage(event_id),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&event_id.to_string()), "{message}");
        assert!(message.contains(&session_id.to_string()), "{message}");
        std::fs::remove_dir_all(directory).ok();
    }
}
